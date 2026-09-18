use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reproit_backend::config::{BackendSdk, ProjectConfig};
use reproit_core::{Error, ErrorCode};
use reproit_sdk_platform::process::ProcessTree;
use reproit_worker::{WorkerSourceFile, WorkerSubject};
use serde::Deserialize;

use crate::source_package::{MAX_SOURCE_BYTES, MAX_SOURCE_FILES, collect_source, verify_revision};

const BUILD_ROOT: &str = ".reproit/build/cargo";
const GUEST_VENDOR: &str = "/source/.reproit/build/cargo/vendor";
const PREPARATION_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_METADATA_BYTES: u64 = 16 * 1_024 * 1_024;
const MAX_STAGING_BYTES: u64 = 1_024 * 1_024 * 1_024;
const MAX_STAGING_ENTRIES: usize = 65_536;

pub(crate) fn collect_execution_source(
    root: &Path,
    revision: &str,
    configuration: &[u8],
    subject: WorkerSubject,
) -> Result<Vec<WorkerSourceFile>, Error> {
    let mut files = collect_source(root, revision)?;
    let project: ProjectConfig = reproit_core::canonical::parse_strict(configuration)?;
    if subject != WorkerSubject::Changed
        || project.sdk != BackendSdk::Rust
        || project.run.program != "cargo"
    {
        return Ok(files);
    }
    if files
        .iter()
        .any(|file| file.path == BUILD_ROOT || file.path.starts_with(&format!("{BUILD_ROOT}/")))
    {
        return Err(dependencies_missing());
    }
    let working = root
        .join(&project.service_path)
        .join(&project.run.working_directory);
    let working = working.canonicalize().map_err(|_| dependencies_missing())?;
    if !working.starts_with(root) {
        return Err(dependencies_missing());
    }
    let staging = tempfile::Builder::new()
        .prefix("reproit-cargo-input-")
        .tempdir()
        .map_err(|_| dependencies_missing())?;
    append_dependencies(root, &working, staging.path(), &mut files)?;
    verify_revision(root, revision)?;
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn append_dependencies(
    root: &Path,
    working: &Path,
    staging: &Path,
    files: &mut Vec<WorkerSourceFile>,
) -> Result<(), Error> {
    let packages = linux_packages(root, working, staging)?;
    let vendor = staging.join("vendor");
    fs::create_dir(&vendor).map_err(|_| dependencies_missing())?;
    let mut command = cargo_command(working);
    command.args([
        "vendor",
        "--locked",
        "--versioned-dirs",
        "--respect-source-config",
    ]);
    command
        .arg("--manifest-path")
        .arg(working.join("Cargo.toml"))
        .arg(&vendor);
    let output = cargo_output(command, 65_536, staging)?;
    let config = vendor_configuration(&output, &vendor)?;
    let mut bytes = files.iter().try_fold(0_usize, |total, file| {
        let decoded = URL_SAFE_NO_PAD
            .decode(&file.bytes)
            .map_err(|_| dependencies_missing())?;
        total.checked_add(decoded.len()).ok_or_else(resource_limit)
    })?;
    append_file(
        files,
        &mut bytes,
        format!("{BUILD_ROOT}/config.toml"),
        config,
        false,
    )?;
    for path in bounded_files(&vendor)? {
        let relative = path
            .strip_prefix(&vendor)
            .map_err(|_| dependencies_missing())?
            .to_str()
            .ok_or_else(dependencies_missing)?
            .replace('\\', "/");
        let (package, member) = relative.split_once('/').ok_or_else(dependencies_missing)?;
        // Cargo resolves every platform before selecting the Linux build. Inactive packages
        // still need their original manifests, checksums, and implicit target entry points.
        if !packages.contains(package) && !resolution_input(member) {
            continue;
        }
        let metadata = path
            .symlink_metadata()
            .map_err(|_| dependencies_missing())?;
        if metadata.len() > (MAX_SOURCE_BYTES.saturating_sub(bytes)) as u64 {
            return Err(resource_limit());
        }
        let contents = fs::read(&path).map_err(|_| dependencies_missing())?;
        append_file(
            files,
            &mut bytes,
            format!("{BUILD_ROOT}/vendor/{relative}"),
            contents,
            executable(&metadata),
        )?;
    }
    Ok(())
}

fn resolution_input(member: &str) -> bool {
    matches!(
        member,
        "Cargo.toml" | ".cargo-checksum.json" | "src/lib.rs" | "src/main.rs" | "build.rs"
    )
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolution,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: PathBuf,
}

#[derive(Deserialize)]
struct Resolution {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
}

fn linux_packages(root: &Path, working: &Path, staging: &Path) -> Result<BTreeSet<String>, Error> {
    let mut selected = BTreeSet::new();
    for target in ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"] {
        let mut command = cargo_command(working);
        command.args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--all-features",
            "--filter-platform",
            target,
        ]);
        command
            .arg("--manifest-path")
            .arg(working.join("Cargo.toml"));
        let metadata: Metadata =
            serde_json::from_slice(&cargo_output(command, MAX_METADATA_BYTES, staging)?)
                .map_err(|_| dependencies_missing())?;
        let nodes: BTreeSet<_> = metadata
            .resolve
            .nodes
            .into_iter()
            .map(|node| node.id)
            .collect();
        for package in metadata.packages {
            if !nodes.contains(&package.id) {
                continue;
            }
            if package.source.is_none() {
                let manifest = package
                    .manifest_path
                    .canonicalize()
                    .map_err(|_| dependencies_missing())?;
                if !manifest.starts_with(root) {
                    return Err(dependencies_missing());
                }
            } else {
                let name = format!("{}-{}", package.name, package.version);
                if !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+".contains(&byte))
                {
                    return Err(dependencies_missing());
                }
                selected.insert(name);
            }
        }
    }
    Ok(selected)
}

fn vendor_configuration(bytes: &[u8], vendor: &Path) -> Result<Vec<u8>, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| dependencies_missing())?;
    let mut config: toml::Table = toml::from_str(text).map_err(|_| dependencies_missing())?;
    if config.keys().any(|key| key != "source") {
        return Err(dependencies_missing());
    }
    if let Some(sources) = config.get_mut("source") {
        for (_, source) in sources
            .as_table_mut()
            .ok_or_else(dependencies_missing)?
            .iter_mut()
        {
            if let Some(directory) = source.get_mut("directory") {
                if directory.as_str() != vendor.to_str() {
                    return Err(dependencies_missing());
                }
                *directory = toml::Value::String(GUEST_VENDOR.to_owned());
            }
        }
    }
    toml::to_string(&config)
        .map(String::into_bytes)
        .map_err(|_| dependencies_missing())
}

fn append_file(
    files: &mut Vec<WorkerSourceFile>,
    total: &mut usize,
    path: String,
    bytes: Vec<u8>,
    executable: bool,
) -> Result<(), Error> {
    *total = total
        .checked_add(bytes.len())
        .filter(|total| *total <= MAX_SOURCE_BYTES)
        .ok_or_else(resource_limit)?;
    if files.len() >= MAX_SOURCE_FILES {
        return Err(resource_limit());
    }
    files.push(WorkerSourceFile {
        bytes: URL_SAFE_NO_PAD.encode(bytes),
        executable,
        path,
    });
    Ok(())
}

fn cargo_command(working: &Path) -> Command {
    let mut command = Command::new("cargo");
    // Cargo reads project configuration from the working directory, even with --manifest-path.
    // Resolve dependencies outside the checkout so its wrappers cannot execute on the developer host.
    let system_root = working.ancestors().last().unwrap_or(working);
    command
        .current_dir(system_root)
        .env("CARGO_TERM_COLOR", "never")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "");
    command
}

fn cargo_output(mut command: Command, limit: u64, staging: &Path) -> Result<Vec<u8>, Error> {
    let mut stdout = tempfile::tempfile_in(staging).map_err(|_| dependencies_missing())?;
    let stderr = tempfile::tempfile_in(staging).map_err(|_| dependencies_missing())?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().map_err(|_| dependencies_missing())?)
        .stderr(stderr.try_clone().map_err(|_| dependencies_missing())?);
    let mut process = ProcessTree::spawn(command).map_err(|_| dependencies_missing())?;
    let deadline = Instant::now() + PREPARATION_TIMEOUT;
    let mut next_disk_check = Instant::now();
    let status = loop {
        if file_size(&stdout)? > limit || file_size(&stderr)? > 1_024 * 1_024 {
            return Err(resource_limit());
        }
        if Instant::now() >= next_disk_check {
            bounded_files(staging)?;
            next_disk_check = Instant::now() + Duration::from_secs(1);
        }
        if let Some(status) = process.try_wait().map_err(|_| dependencies_missing())? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(resource_limit());
        }
        thread::sleep(Duration::from_millis(10));
    };
    process.terminate().map_err(|_| dependencies_missing())?;
    if !status.success() {
        return Err(dependencies_missing());
    }
    stdout
        .seek(SeekFrom::Start(0))
        .map_err(|_| dependencies_missing())?;
    let mut output = Vec::new();
    stdout
        .take(limit + 1)
        .read_to_end(&mut output)
        .map_err(|_| dependencies_missing())?;
    if output.len() as u64 > limit {
        return Err(resource_limit());
    }
    Ok(output)
}

fn file_size(file: &File) -> Result<u64, Error> {
    file.metadata()
        .map(|metadata| metadata.len())
        .map_err(|_| dependencies_missing())
}

fn bounded_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut pending = vec![root.to_owned()];
    let mut files = Vec::new();
    let mut entries = 0_usize;
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|_| dependencies_missing())? {
            let entry = entry.map_err(|_| dependencies_missing())?;
            entries += 1;
            let metadata = entry
                .path()
                .symlink_metadata()
                .map_err(|_| dependencies_missing())?;
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or_else(resource_limit)?;
            if entries > MAX_STAGING_ENTRIES || bytes > MAX_STAGING_BYTES {
                return Err(resource_limit());
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                files.push(entry.path());
            } else {
                return Err(dependencies_missing());
            }
        }
    }
    files.sort_unstable();
    Ok(files)
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &fs::Metadata) -> bool {
    false
}

fn dependencies_missing() -> Error {
    Error::new(
        ErrorCode::SourceDependencyMissing,
        "Repro It could not prepare the locked Cargo dependencies for isolated replay.",
    )
}

fn resource_limit() -> Error {
    Error::new(
        ErrorCode::RuntimeQuota,
        "The Cargo replay inputs exceed the preparation limits.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_package::tests::{head, initialize_repository, run_git};

    #[test]
    fn cargo_inputs_preserve_source_and_ignore_checkout_wrappers() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        initialize_repository(&root);
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='replay-build-test'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let mut command = cargo_command(&root);
        command
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(root.join("Cargo.toml"));
        let staging = tempfile::tempdir().unwrap();
        cargo_output(command, 65_536, staging.path()).unwrap();
        fs::create_dir(root.join(".cargo")).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper='a-wrapper-that-must-not-run'\n",
        )
        .unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "Cargo source"]);
        let revision = head(&root);
        let original = collect_source(&root, &revision).unwrap().len();
        let configuration = serde_json::to_vec(&serde_json::json!({
            "format": 1, "profile": "backend", "profile_format": 1,
            "processing_mode": "managed", "sdk": "rust",
            "organization_id": "org_01890f3e-7b1c-7cc0-8a1b-123456789abd",
            "project_id": "prj_01890f3e-7b1c-7cc0-8a1b-123456789abe",
            "service_id": "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf",
            "repository_id": "source.example/acme/commerce", "service_path": ".",
            "source": {"remote": "origin"},
            "run": {"program": "cargo", "arguments": ["run"], "working_directory": "."}
        }))
        .unwrap();
        let files =
            collect_execution_source(&root, &revision, &configuration, WorkerSubject::Changed)
                .unwrap();
        assert_eq!(files.len(), original + 1);
        assert!(
            files
                .iter()
                .any(|file| file.path == format!("{BUILD_ROOT}/config.toml"))
        );
        verify_revision(&root, &revision).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { panic!(); }\n").unwrap();
        assert_eq!(
            verify_revision(&root, &revision).unwrap_err().code,
            ErrorCode::SourceRevisionMissing
        );
    }

    #[test]
    fn dependency_configuration_cannot_redirect_the_guest_vendor_directory() {
        let config = b"[source.crates-io]\nreplace-with='vendored-sources'\n[source.vendored-sources]\ndirectory='/staging/vendor'\n";
        let bytes = vendor_configuration(config, Path::new("/staging/vendor")).unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains(GUEST_VENDOR));
        assert!(vendor_configuration(config, Path::new("/different/vendor")).is_err());
        assert!(
            vendor_configuration(b"[build]\nrustc-wrapper='unexpected'", Path::new("/vendor"))
                .is_err()
        );
        #[cfg(unix)]
        {
            let root = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("/outside", root.path().join("link")).unwrap();
            assert!(bounded_files(root.path()).is_err());
        }
        let mut files = Vec::new();
        let mut total = MAX_SOURCE_BYTES;
        append_file(
            &mut files,
            &mut total,
            "empty".to_owned(),
            Vec::new(),
            false,
        )
        .unwrap();
        assert!(append_file(&mut files, &mut total, "over".to_owned(), vec![0], false).is_err());
    }

    #[test]
    fn offline_cargo_resolves_inactive_packages_with_implicit_targets() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            root.join("Cargo.toml"),
            concat!(
                "[package]\nname='offline-resolver-test'\nversion='0.1.0'\n",
                "[target.'cfg(target_os = \"none\")'.dependencies]\ninactive='=1.0.0'\n"
            ),
        )
        .unwrap();
        let vendor = root.join("vendor");
        let package = vendor.join("inactive-1.0.0");
        fs::create_dir_all(package.join("src")).unwrap();
        for (path, bytes) in [
            (
                "Cargo.toml",
                "[package]\nname='inactive'\nversion='1.0.0'\n",
            ),
            (".cargo-checksum.json", "{\"files\":{},\"package\":null}"),
            ("src/lib.rs", "mod inactive_implementation;\n"),
            ("src/inactive_implementation.rs", "pub fn unused() {}\n"),
        ] {
            if resolution_input(path) {
                fs::write(package.join(path), bytes).unwrap();
            }
        }
        assert!(!package.join("src/inactive_implementation.rs").exists());
        let vendor_path = vendor.to_str().unwrap();
        let config = toml::toml! {
            [source.crates-io]
            replace-with = "offline"
            [source.offline]
            directory = vendor_path
        };
        fs::write(root.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
        let mut command = cargo_command(&root);
        command
            .arg("--config")
            .arg(root.join("config.toml"))
            .args(["check", "--offline", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", root.join("target"));
        let staging = tempfile::tempdir().unwrap();
        cargo_output(command, 65_536, staging.path()).unwrap();
    }
}
