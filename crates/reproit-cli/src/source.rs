use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use reproit_backend::config::ProjectConfig;
use reproit_core::{Error, ErrorCode, model::SourcePreparationPolicy};

const GIT_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_GIT_OUTPUT_BYTES: usize = 2_048;
const MAX_SOURCE_AUTHORIZATION_BYTES: u64 = 65_536;
static ACTIVE_SOURCE_JOBS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GitCheckout {
    pub origin: String,
    pub repository_id: String,
    pub source_revision: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GitRepositoryIdentity {
    pub remote: String,
    pub repository_id: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ManagedSource {
    pub repository_id: String,
    pub source_revision: String,
    pub workspace: String,
}

pub struct SourceCheckout {
    pub repository_id: String,
    pub source_revision: String,
}

pub fn current_git_repository(path: &Path) -> Result<GitRepositoryIdentity, Error> {
    let root = PathBuf::from(git_output(path, &["rev-parse", "--show-toplevel"])?);
    if !root.is_absolute() || !root.is_dir() || !path.starts_with(&root) {
        return Err(source_denied());
    }
    let remote = "origin".to_owned();
    let origin = git_output(&root, &["remote", "get-url", &remote])?;
    Ok(GitRepositoryIdentity {
        remote,
        repository_id: canonical_repository_identity(&origin)?,
        root,
    })
}

pub struct GitSourceWorkspace {
    origin: String,
    root: tempfile::TempDir,
}

impl GitSourceWorkspace {
    pub fn new(root: &Path, project: &ProjectConfig) -> Result<Self, Error> {
        let origin = git_output(root, &["remote", "get-url", &project.source.remote])?;
        if canonical_repository_identity(&origin)? != project.repository_id {
            return Err(source_denied());
        }
        tempfile::Builder::new()
            .prefix("reproit-source-")
            .tempdir()
            .map(|root| Self { origin, root })
            .map_err(|_| checkout_failed())
    }

    pub fn cancel(&self, source: &ManagedSource) -> Result<(), Error> {
        let expected = self.root.path().join("checkout");
        if Path::new(&source.workspace) != expected {
            return Err(checkout_failed());
        }
        match fs::remove_dir_all(expected) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(checkout_failed()),
        }
    }
}

impl GitSourceWorkspace {
    pub fn prepare(&self, checkout: &SourceCheckout) -> Result<ManagedSource, Error> {
        let target = self.root.path().join("checkout");
        checkout_git(
            &GitCheckout {
                origin: self.origin.clone(),
                repository_id: checkout.repository_id.clone(),
                source_revision: checkout.source_revision.clone(),
            },
            &target,
        )?;
        Ok(ManagedSource {
            repository_id: checkout.repository_id.clone(),
            source_revision: checkout.source_revision.clone(),
            workspace: target.to_string_lossy().into_owned(),
        })
    }

    pub fn cleanup(&self, source: &ManagedSource) -> Result<(), Error> {
        self.cancel(source)
    }
}

pub fn current_project_source(
    root: &Path,
    project: &ProjectConfig,
) -> Result<ManagedSource, Error> {
    let remote = git_output(root, &["remote", "get-url", &project.source.remote])?;
    if canonical_repository_identity(&remote)? != project.repository_id {
        return Err(source_denied());
    }
    let revision = git_output(root, &["rev-parse", "HEAD"])?;
    if !valid_revision(&revision) {
        return Err(Error::new(
            ErrorCode::SourceRevisionMissing,
            "The active Git source revision is not immutable.",
        ));
    }
    Ok(ManagedSource {
        repository_id: project.repository_id.clone(),
        source_revision: revision,
        workspace: root.to_string_lossy().into_owned(),
    })
}

pub fn checkout_git(checkout: &GitCheckout, target: &Path) -> Result<(), Error> {
    validate_checkout(checkout, target)?;
    let _reservation = SourceReservation::acquire()?;
    let deadline = Instant::now()
        .checked_add(GIT_TIMEOUT)
        .ok_or_else(checkout_failed)?;
    run_git(
        git_command().arg("init").arg("--quiet").arg(target),
        None,
        deadline,
        None,
    )?;
    let result = checkout_exact_revision(checkout, target, deadline);
    if result.is_err() {
        let _ = fs::remove_dir_all(target);
    }
    result
}

fn checkout_exact_revision(
    checkout: &GitCheckout,
    target: &Path,
    deadline: Instant,
) -> Result<(), Error> {
    run_git(
        git_command()
            .arg("-C")
            .arg(target)
            .args(["remote", "add", "origin"])
            .arg(&checkout.origin),
        None,
        deadline,
        None,
    )?;
    run_git(
        git_command()
            .arg("-C")
            .arg(target)
            .args(["fetch", "--depth=1", "--no-tags", "origin"])
            .arg(&checkout.source_revision),
        None,
        deadline,
        Some(target),
    )?;
    run_git(
        git_command()
            .arg("-C")
            .arg(target)
            .args(["checkout", "--quiet", "--detach", "FETCH_HEAD"]),
        None,
        deadline,
        Some(target),
    )?;
    let actual = git_output(target, &["rev-parse", "HEAD"])?;
    if actual != checkout.source_revision {
        return Err(Error::new(
            ErrorCode::SourceRevisionMissing,
            "Git did not check out the required source revision.",
        ));
    }
    prepare_submodules(target, deadline)?;
    prepare_lfs(target, deadline)?;
    validate_source_usage(target)?;
    Ok(())
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command.env("GIT_LFS_SKIP_SMUDGE", "1");
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "diff.external=",
        "-c",
        "filter.lfs.required=false",
        "-c",
        "filter.lfs.smudge=",
        "-c",
        "filter.lfs.process=",
        "-c",
        "http.followRedirects=false",
        "-c",
        "submodule.recurse=false",
    ]);
    command
}

struct SourceReservation;

impl SourceReservation {
    fn acquire() -> Result<Self, Error> {
        let concurrent_jobs = policy_limit(SourcePreparationPolicy::V1.concurrent_jobs)?;
        ACTIVE_SOURCE_JOBS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < concurrent_jobs).then_some(active + 1)
            })
            .map(|_| Self)
            .map_err(|_| {
                Error::new(
                    ErrorCode::RateLimited,
                    "The source preparation capacity is full.",
                )
            })
    }
}

impl Drop for SourceReservation {
    fn drop(&mut self) {
        ACTIVE_SOURCE_JOBS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceAuthorizations {
    format: String,
    repositories: BTreeSet<String>,
}

fn prepare_submodules(target: &Path, deadline: Instant) -> Result<(), Error> {
    if !target.join(".gitmodules").exists() {
        return Ok(());
    }
    let authorizations = source_authorizations()?;
    let maximum_depth = policy_limit(SourcePreparationPolicy::V1.submodule_depth)?;
    let maximum_submodules = policy_limit(SourcePreparationPolicy::V1.submodules)?;
    let mut pending = VecDeque::from([(target.to_path_buf(), 1_usize)]);
    let mut total = 0_usize;
    while let Some((repository, depth)) = pending.pop_front() {
        if !repository.join(".gitmodules").exists() {
            continue;
        }
        if depth > maximum_depth {
            return Err(source_limit());
        }
        let declarations = declared_submodules(&repository)?;
        for (_, origin) in &declarations {
            if !authorizations.contains(&canonical_repository_identity(origin)?) {
                return Err(source_denied());
            }
        }
        for (path, _) in declarations {
            total = total.checked_add(1).ok_or_else(source_limit)?;
            if total > maximum_submodules {
                return Err(source_limit());
            }
            run_git(
                git_command()
                    .arg("-C")
                    .arg(&repository)
                    .args(["submodule", "update", "--init", "--depth=1", "--"])
                    .arg(&path),
                None,
                deadline,
                Some(target),
            )?;
            pending.push_back((repository.join(path), depth + 1));
        }
    }
    validate_source_usage(target)
}

fn declared_submodules(repository: &Path) -> Result<Vec<(PathBuf, String)>, Error> {
    let maximum_submodules = policy_limit(SourcePreparationPolicy::V1.submodules)?;
    let maximum_output = MAX_GIT_OUTPUT_BYTES
        .checked_mul(maximum_submodules)
        .ok_or_else(source_limit)?;
    let output = git_output_bounded(
        repository,
        &[
            "config",
            "--file",
            ".gitmodules",
            "--get-regexp",
            "^submodule\\..*\\..*$",
        ],
        maximum_output,
    )?;
    let mut declarations = BTreeMap::<String, (Option<PathBuf>, Option<String>)>::new();
    for line in output.lines() {
        let (key, value) = line.split_once(' ').ok_or_else(checkout_failed)?;
        let (name, field) = key
            .strip_prefix("submodule.")
            .and_then(|value| value.rsplit_once('.'))
            .ok_or_else(checkout_failed)?;
        let declaration = declarations.entry(name.to_owned()).or_default();
        match field {
            "path" => declaration.0 = Some(valid_submodule_path(value)?),
            "url" => declaration.1 = Some(value.to_owned()),
            _ => return Err(checkout_failed()),
        }
    }
    declarations
        .into_values()
        .map(|(path, origin)| {
            Ok((
                path.ok_or_else(checkout_failed)?,
                origin.ok_or_else(checkout_failed)?,
            ))
        })
        .collect()
}

fn valid_submodule_path(value: &str) -> Result<PathBuf, Error> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 1_024
        || path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(checkout_failed());
    }
    Ok(path.to_path_buf())
}

fn source_authorizations() -> Result<BTreeSet<String>, Error> {
    let path = std::env::var_os("REPROIT_SOURCE_AUTHORIZATION_FILE")
        .map(PathBuf::from)
        .ok_or_else(source_denied)?;
    validate_protected_file(&path, MAX_SOURCE_AUTHORIZATION_BYTES)?;
    let bytes = fs::read(path).map_err(|_| source_denied())?;
    let value: SourceAuthorizations =
        serde_json::from_slice(&bytes).map_err(|_| source_denied())?;
    let maximum_repositories = policy_limit(SourcePreparationPolicy::V1.submodules)?;
    if value.format != "reproit.source-authorizations.v1"
        || value.repositories.is_empty()
        || value.repositories.len() > maximum_repositories
        || value.repositories.iter().any(|repository| {
            !canonical_repository_identity(&format!("https://{repository}"))
                .is_ok_and(|identity| identity == *repository)
        })
    {
        return Err(source_denied());
    }
    Ok(value.repositories)
}

fn validate_protected_file(path: &Path, maximum_bytes: u64) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(source_denied());
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| source_denied())?;
    if !metadata.file_type().is_file() || metadata.len() > maximum_bytes {
        return Err(source_denied());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(source_denied());
        }
    }
    Ok(())
}

fn prepare_lfs(target: &Path, deadline: Instant) -> Result<(), Error> {
    let pointers = lfs_pointers(target)?;
    if pointers.is_empty() {
        return Ok(());
    }
    for repository in repositories(target)? {
        run_git(
            git_command()
                .arg("-C")
                .arg(repository)
                .args(["lfs", "pull"]),
            None,
            deadline,
            Some(target),
        )?;
    }
    validate_source_usage(target)
}

fn lfs_pointers(target: &Path) -> Result<Vec<u64>, Error> {
    let maximum_objects = policy_limit(SourcePreparationPolicy::V1.git_lfs_objects)?;
    let mut sizes = Vec::new();
    let mut total_bytes = 0_u64;
    for file in bounded_files(target)? {
        let metadata = fs::symlink_metadata(&file).map_err(|_| checkout_failed())?;
        if !metadata.file_type().is_file() || metadata.len() > 1_024 {
            continue;
        }
        let bytes = fs::read(&file).map_err(|_| checkout_failed())?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if !text.starts_with("version https://git-lfs.github.com/spec/v1\n") {
            continue;
        }
        let size = text
            .lines()
            .find_map(|line| line.strip_prefix("size "))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(checkout_failed)?;
        if size > SourcePreparationPolicy::V1.git_lfs_object_bytes {
            return Err(source_limit());
        }
        total_bytes = total_bytes.checked_add(size).ok_or_else(source_limit)?;
        sizes.push(size);
        if sizes.len() > maximum_objects || total_bytes > SourcePreparationPolicy::V1.git_lfs_bytes
        {
            return Err(source_limit());
        }
    }
    Ok(sizes)
}

fn repositories(target: &Path) -> Result<Vec<std::path::PathBuf>, Error> {
    let mut repositories = vec![target.to_path_buf()];
    for path in bounded_files(target)? {
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            let parent = path.parent().ok_or_else(checkout_failed)?;
            if parent != target {
                repositories.push(parent.to_path_buf());
            }
        }
    }
    repositories.sort();
    repositories.dedup();
    Ok(repositories)
}

fn validate_source_usage(target: &Path) -> Result<(), Error> {
    let files = bounded_files(target)?;
    let mut bytes = 0_u64;
    for path in &files {
        let metadata = fs::symlink_metadata(path).map_err(|_| checkout_failed())?;
        if metadata.file_type().is_file() {
            bytes = bytes.checked_add(metadata.len()).ok_or_else(source_limit)?;
        }
    }
    if files.len() > policy_limit(SourcePreparationPolicy::V1.filesystem_entries)?
        || bytes > SourcePreparationPolicy::V1.source_bytes
    {
        return Err(source_limit());
    }
    Ok(())
}

fn bounded_files(root: &Path) -> Result<Vec<std::path::PathBuf>, Error> {
    let maximum_entries = policy_limit(SourcePreparationPolicy::V1.filesystem_entries)?;
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|_| checkout_failed())? {
            let path = entry.map_err(|_| checkout_failed())?.path();
            files.push(path.clone());
            if files.len() > maximum_entries {
                return Err(source_limit());
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| checkout_failed())?;
            if metadata.file_type().is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(files)
}

fn git_output(root: &Path, arguments: &[&str]) -> Result<String, Error> {
    git_output_bounded(root, arguments, MAX_GIT_OUTPUT_BYTES)
}

fn git_output_bounded(root: &Path, arguments: &[&str], max_bytes: usize) -> Result<String, Error> {
    let output = git_command()
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|_| checkout_failed())?;
    if !output.status.success() || !output.stderr.is_empty() || output.stdout.len() > max_bytes {
        return Err(checkout_failed());
    }
    let value = std::str::from_utf8(&output.stdout)
        .map_err(|_| checkout_failed())?
        .trim();
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(checkout_failed());
    }
    Ok(value.to_owned())
}

fn run_git(
    command: &mut Command,
    working_directory: Option<&Path>,
    deadline: Instant,
    usage_root: Option<&Path>,
) -> Result<(), Error> {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }
    let child = command.spawn().map_err(|_| checkout_failed())?;
    wait_for_git(child, deadline, usage_root)
}

fn wait_for_git(
    mut child: Child,
    deadline: Instant,
    usage_root: Option<&Path>,
) -> Result<(), Error> {
    let mut next_usage_check = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|_| checkout_failed())? {
            if status.success() {
                if let Some(root) = usage_root {
                    validate_source_usage(root)?;
                }
                return Ok(());
            }
            return Err(checkout_failed());
        }
        if Instant::now() >= deadline {
            child.kill().map_err(|_| checkout_failed())?;
            let _ = child.wait();
            return Err(checkout_failed());
        }
        if let Some(root) = usage_root
            && Instant::now() >= next_usage_check
        {
            if validate_source_usage(root).is_err() {
                child.kill().map_err(|_| checkout_failed())?;
                let _ = child.wait();
                return Err(source_limit());
            }
            next_usage_check = Instant::now()
                .checked_add(Duration::from_millis(250))
                .ok_or_else(checkout_failed)?;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn validate_checkout(checkout: &GitCheckout, target: &Path) -> Result<(), Error> {
    if canonical_repository_identity(&checkout.origin)? != checkout.repository_id
        || !valid_revision(&checkout.source_revision)
        || target.as_os_str().is_empty()
        || target.exists()
        || target.parent().is_none_or(|parent| !parent.is_dir())
    {
        return Err(Error::schema_invalid());
    }
    Ok(())
}

fn canonical_repository_identity(origin: &str) -> Result<String, Error> {
    if origin.bytes().any(|byte| byte.is_ascii_control()) || origin.len() > 2_048 {
        return Err(source_denied());
    }
    if let Some(value) = origin.strip_prefix("git@") {
        let (host, path) = value.split_once(':').ok_or_else(source_denied)?;
        return canonical_identity_parts(host, path);
    }
    let url = reqwest::Url::parse(origin).map_err(|_| source_denied())?;
    if !matches!(url.scheme(), "https" | "ssh")
        || url.host_str().is_none()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.scheme() == "https" && !url.username().is_empty())
        || (url.scheme() == "ssh" && !matches!(url.username(), "" | "git"))
    {
        return Err(source_denied());
    }
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().expect("validated host")),
        None => url.host_str().expect("validated host").to_owned(),
    };
    canonical_identity_parts(&host, url.path())
}

fn canonical_identity_parts(host: &str, path: &str) -> Result<String, Error> {
    let repository = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path.trim_matches('/'));
    if host.is_empty()
        || repository.is_empty()
        || host.len().saturating_add(repository.len()) > 255
        || !host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        })
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'/'))
    {
        return Err(source_denied());
    }
    Ok(format!("{}/{repository}", host.to_ascii_lowercase()))
}

fn valid_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn policy_limit(value: u64) -> Result<usize, Error> {
    usize::try_from(value).map_err(|_| source_limit())
}

fn source_denied() -> Error {
    Error::new(
        ErrorCode::SourceAccessDenied,
        "The Git origin does not match the authorized repository identity.",
    )
}

fn checkout_failed() -> Error {
    Error::new(
        ErrorCode::SourceCheckoutFailed,
        "The bounded Git source checkout failed.",
    )
}

fn source_limit() -> Error {
    Error::new(
        ErrorCode::RuntimeQuota,
        "The source preparation limit was reached.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_https_and_ssh_repository_identity() {
        for origin in [
            "https://source.example/acme/commerce.git",
            "ssh://git@source.example/acme/commerce.git",
            "git@source.example:acme/commerce.git",
        ] {
            assert_eq!(
                canonical_repository_identity(origin).expect("repository identity"),
                "source.example/acme/commerce"
            );
        }
    }

    #[test]
    fn rejects_credentials_and_mutable_revisions() {
        assert!(canonical_repository_identity("https://token@source.example/a/b.git").is_err());
        let root = tempfile::tempdir().expect("temporary root");
        let checkout = GitCheckout {
            origin: "https://source.example/acme/commerce.git".to_owned(),
            repository_id: "source.example/acme/commerce".to_owned(),
            source_revision: "main".to_owned(),
        };
        assert!(validate_checkout(&checkout, &root.path().join("checkout")).is_err());
    }

    #[test]
    fn source_reservation_rejects_before_a_third_job() {
        let first = SourceReservation::acquire().expect("first source reservation");
        let second = SourceReservation::acquire().expect("second source reservation");
        assert_eq!(
            SourceReservation::acquire()
                .err()
                .expect("third rejection")
                .code,
            ErrorCode::RateLimited
        );
        drop(first);
        SourceReservation::acquire().expect("released source reservation");
        drop(second);
    }

    #[test]
    fn managed_checkout_cancellation_is_bounded_and_idempotent() {
        let root = tempfile::tempdir().expect("managed source root");
        let checkout = root.path().join("checkout");
        fs::create_dir(&checkout).expect("managed checkout");
        fs::write(checkout.join("source"), b"immutable").expect("managed source file");
        let workspace = GitSourceWorkspace {
            origin: "https://source.example/acme/commerce.git".to_owned(),
            root,
        };
        let source = ManagedSource {
            repository_id: "source.example/acme/commerce".to_owned(),
            source_revision: "a".repeat(40),
            workspace: checkout.to_string_lossy().into_owned(),
        };

        workspace.cancel(&source).expect("first cleanup");
        workspace.cancel(&source).expect("idempotent cleanup");

        let mut wrong = source;
        wrong.workspace = workspace
            .root
            .path()
            .join("other")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            workspace.cancel(&wrong).expect_err("wrong checkout").code,
            ErrorCode::SourceCheckoutFailed
        );
    }

    #[test]
    fn lfs_pointer_size_is_rejected_before_lfs_network_access() {
        let root = tempfile::tempdir().expect("source fixture");
        fs::write(
            root.path().join("large.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\n\
                 oid sha256:{}\nsize {}\n",
                "a".repeat(64),
                SourcePreparationPolicy::V1.git_lfs_object_bytes + 1
            ),
        )
        .expect("write LFS pointer");
        assert_eq!(
            lfs_pointers(root.path())
                .expect_err("oversized pointer")
                .code,
            ErrorCode::RuntimeQuota
        );
    }

    #[test]
    fn submodule_custom_update_is_rejected_before_fetch() {
        let root = tempfile::tempdir().expect("submodule fixture");
        fs::write(
            root.path().join(".gitmodules"),
            "[submodule \"dependency\"]\n\
             path = dependency\n\
             url = https://source.example/acme/dependency.git\n\
             update = !false\n",
        )
        .expect("write submodule declaration");
        assert_eq!(
            declared_submodules(root.path())
                .expect_err("custom update command")
                .code,
            ErrorCode::SourceCheckoutFailed
        );
    }

    #[test]
    fn source_growth_stops_a_running_effect_at_one_byte_over() {
        let root = tempfile::tempdir().expect("source fixture");
        let oversized = fs::File::create(root.path().join("oversized.pack"))
            .expect("create sparse source object");
        oversized
            .set_len(SourcePreparationPolicy::V1.source_bytes + 1)
            .expect("size sparse source object");
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 1"]);
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .expect("deadline");
        assert_eq!(
            run_git(&mut command, None, deadline, Some(root.path()))
                .expect_err("source limit")
                .code,
            ErrorCode::RuntimeQuota
        );
    }
}
