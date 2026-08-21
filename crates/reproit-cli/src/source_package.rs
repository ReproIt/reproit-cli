use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reproit_core::{Error, ErrorCode};
use reproit_worker::WorkerSourceFile;

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_GIT_ERROR_BYTES: usize = 2_048;
const MAX_GIT_REVISION_BYTES: usize = 128;
const MAX_SOURCE_BYTES: usize = 256 * 1_024 * 1_024;
const MAX_SOURCE_FILES: usize = 16_384;
const MAX_SOURCE_PATH_BYTES: usize = 4_096;
const MAX_TREE_BYTES: usize = MAX_SOURCE_FILES * (MAX_SOURCE_PATH_BYTES + 128);
const MAX_BATCH_OVERHEAD_BYTES: usize = MAX_SOURCE_FILES * 128;

pub(crate) fn collect_source(
    root: &Path,
    expected_revision: &str,
) -> Result<Vec<WorkerSourceFile>, Error> {
    validate_root(root)?;
    require_revision(root, expected_revision)?;
    require_clean_checkout(root)?;
    let entries = revision_entries(root, expected_revision)?;
    let files = read_revision_blobs(root, &entries)?;
    require_revision(root, expected_revision)?;
    require_clean_checkout(root)?;
    Ok(files)
}

fn validate_root(root: &Path) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(root).map_err(|_| source_invalid())?;
    if !root.is_absolute() || !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(source_invalid());
    }
    Ok(())
}

fn require_revision(root: &Path, expected_revision: &str) -> Result<(), Error> {
    if expected_revision.is_empty() || expected_revision.len() > MAX_GIT_REVISION_BYTES {
        return Err(source_changed());
    }
    let bytes = git_output(
        root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        &[],
        MAX_GIT_REVISION_BYTES,
    )
    .map_err(map_revision_error)?;
    let actual = std::str::from_utf8(&bytes)
        .map_err(|_| source_checkout_failed())?
        .trim_end_matches(['\r', '\n']);
    if actual != expected_revision {
        return Err(source_changed());
    }
    Ok(())
}

fn require_clean_checkout(root: &Path) -> Result<(), Error> {
    match git_output(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ],
        &[],
        0,
    ) {
        Ok(bytes) if bytes.is_empty() => Ok(()),
        Err(GitOutputError::Limit) | Ok(_) => Err(source_changed()),
        Err(GitOutputError::Command) => Err(source_checkout_failed()),
    }
}

#[derive(Clone, Debug)]
struct RevisionEntry {
    executable: bool,
    object_id: String,
    path: String,
}

fn revision_entries(root: &Path, revision: &str) -> Result<Vec<RevisionEntry>, Error> {
    let bytes = git_output(
        root,
        &["ls-tree", "-r", "-z", "--full-tree", revision],
        &[],
        MAX_TREE_BYTES,
    )
    .map_err(|error| match error {
        GitOutputError::Command => source_checkout_failed(),
        GitOutputError::Limit => source_too_large(),
    })?;
    parse_tree(&bytes)
}

fn parse_tree(bytes: &[u8]) -> Result<Vec<RevisionEntry>, Error> {
    if bytes.is_empty() || !bytes.ends_with(&[0]) {
        return Err(source_invalid());
    }
    let mut paths = BTreeSet::new();
    let mut entries = Vec::new();
    for raw in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        let separator = raw
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(source_invalid)?;
        let metadata = std::str::from_utf8(&raw[..separator]).map_err(|_| source_invalid())?;
        let mut fields = metadata.split(' ');
        let mode = fields.next().ok_or_else(source_invalid)?;
        let kind = fields.next().ok_or_else(source_invalid)?;
        let object_id = fields.next().ok_or_else(source_invalid)?;
        if fields.next().is_some()
            || kind != "blob"
            || !matches!(mode, "100644" | "100755")
            || !valid_object_id(object_id)
        {
            return Err(source_invalid());
        }
        let path = std::str::from_utf8(&raw[separator + 1..]).map_err(|_| source_invalid())?;
        if !valid_source_path(path) || !paths.insert(path.to_owned()) {
            return Err(source_invalid());
        }
        entries.push(RevisionEntry {
            executable: mode == "100755",
            object_id: object_id.to_owned(),
            path: path.to_owned(),
        });
        if entries.len() > MAX_SOURCE_FILES {
            return Err(source_too_large());
        }
    }
    if entries.is_empty() {
        return Err(source_invalid());
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn valid_source_path(path: &str) -> bool {
    if path.is_empty()
        || path.len() > MAX_SOURCE_PATH_BYTES
        || path.contains('\\')
        || Path::new(path).is_absolute()
    {
        return false;
    }
    Path::new(path).components().all(|component| {
        matches!(component, Component::Normal(_)) && component.as_os_str() != ".git"
    })
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_revision_blobs(
    root: &Path,
    entries: &[RevisionEntry],
) -> Result<Vec<WorkerSourceFile>, Error> {
    let mut input = Vec::with_capacity(entries.len() * 65);
    for entry in entries {
        input.extend_from_slice(entry.object_id.as_bytes());
        input.push(b'\n');
    }
    let maximum_bytes = MAX_SOURCE_BYTES
        .checked_add(MAX_BATCH_OVERHEAD_BYTES)
        .ok_or_else(source_too_large)?;
    let mut output =
        git_output(root, &["cat-file", "--batch"], &input, maximum_bytes).map_err(|error| {
            match error {
                GitOutputError::Command => source_checkout_failed(),
                GitOutputError::Limit => source_too_large(),
            }
        })?;
    input.fill(0);
    parse_batch(&mut output, entries)
}

fn parse_batch(
    output: &mut [u8],
    entries: &[RevisionEntry],
) -> Result<Vec<WorkerSourceFile>, Error> {
    let result = parse_batch_inner(output, entries);
    output.fill(0);
    result
}

fn parse_batch_inner(
    output: &[u8],
    entries: &[RevisionEntry],
) -> Result<Vec<WorkerSourceFile>, Error> {
    let mut cursor = 0_usize;
    let mut total_bytes = 0_usize;
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        let header_end = output[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .and_then(|offset| cursor.checked_add(offset))
            .ok_or_else(source_invalid)?;
        let header =
            std::str::from_utf8(&output[cursor..header_end]).map_err(|_| source_invalid())?;
        let mut fields = header.split(' ');
        let object_id = fields.next().ok_or_else(source_invalid)?;
        let kind = fields.next().ok_or_else(source_invalid)?;
        let size = fields
            .next()
            .ok_or_else(source_invalid)?
            .parse::<usize>()
            .map_err(|_| source_invalid())?;
        if fields.next().is_some() || object_id != entry.object_id || kind != "blob" {
            return Err(source_invalid());
        }
        total_bytes = total_bytes
            .checked_add(size)
            .filter(|value| *value <= MAX_SOURCE_BYTES)
            .ok_or_else(source_too_large)?;
        let start = header_end.checked_add(1).ok_or_else(source_too_large)?;
        let end = start.checked_add(size).ok_or_else(source_too_large)?;
        if output.get(end) != Some(&b'\n') {
            return Err(source_invalid());
        }
        let bytes = output.get(start..end).ok_or_else(source_invalid)?;
        files.push(WorkerSourceFile {
            bytes: URL_SAFE_NO_PAD.encode(bytes),
            executable: entry.executable,
            path: entry.path.clone(),
        });
        cursor = end.checked_add(1).ok_or_else(source_too_large)?;
    }
    if cursor != output.len() {
        return Err(source_invalid());
    }
    Ok(files)
}

#[derive(Clone, Copy)]
enum GitOutputError {
    Command,
    Limit,
}

fn git_output(
    root: &Path,
    arguments: &[&str],
    input: &[u8],
    maximum_bytes: usize,
) -> Result<Vec<u8>, GitOutputError> {
    let mut input_file = tempfile::tempfile().map_err(|_| GitOutputError::Command)?;
    input_file
        .write_all(input)
        .and_then(|()| input_file.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|_| GitOutputError::Command)?;
    let mut command = sanitized_git(root);
    let mut child = command
        .args(arguments)
        .stdin(Stdio::from(input_file))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| GitOutputError::Command)?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let stdout = bounded_reader(
        child.stdout.take().ok_or(GitOutputError::Command)?,
        maximum_bytes,
        exceeded.clone(),
    );
    let stderr = bounded_reader(
        child.stderr.take().ok_or(GitOutputError::Command)?,
        MAX_GIT_ERROR_BYTES,
        exceeded.clone(),
    );
    let status = wait_for_git_output(&mut child, &exceeded);
    let stdout = stdout
        .join()
        .map_err(|_| GitOutputError::Command)?
        .map_err(|_| GitOutputError::Command)?;
    let stderr = stderr
        .join()
        .map_err(|_| GitOutputError::Command)?
        .map_err(|_| GitOutputError::Command)?;
    if exceeded.load(Ordering::Acquire) {
        return Err(GitOutputError::Limit);
    }
    let status = status?;
    if !status.success() || !stderr.is_empty() {
        return Err(GitOutputError::Command);
    }
    Ok(stdout)
}

fn sanitized_git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.untrackedCache=false")
        .arg("-c")
        .arg("core.hooksPath=")
        .arg("-C")
        .arg(root)
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_SHALLOW_FILE",
        "GIT_WORK_TREE",
    ] {
        command.env_remove(name);
    }
    command
}

fn wait_for_git_output(
    child: &mut Child,
    exceeded: &AtomicBool,
) -> Result<ExitStatus, GitOutputError> {
    let deadline = Instant::now()
        .checked_add(GIT_COMMAND_TIMEOUT)
        .ok_or(GitOutputError::Command)?;
    loop {
        if exceeded.load(Ordering::Acquire) {
            stop_child(child);
            return Err(GitOutputError::Limit);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                stop_child(child);
                return Err(GitOutputError::Command);
            }
        }
    }
}

fn bounded_reader(
    mut reader: impl Read + Send + 'static,
    maximum_bytes: usize,
    exceeded: Arc<AtomicBool>,
) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0_u8; 8_192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(kept);
            }
            if kept.len().saturating_add(count) > maximum_bytes {
                exceeded.store(true, Ordering::Release);
                return Ok(kept);
            }
            kept.extend_from_slice(&buffer[..count]);
        }
    })
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn map_revision_error(error: GitOutputError) -> Error {
    match error {
        GitOutputError::Command => source_checkout_failed(),
        GitOutputError::Limit => source_changed(),
    }
}

fn source_invalid() -> Error {
    Error::new(
        ErrorCode::SchemaInvalid,
        "The Git source closure is invalid.",
    )
}

fn source_changed() -> Error {
    Error::new(
        ErrorCode::SourceRevisionMissing,
        "The Git checkout does not match its immutable source revision.",
    )
}

fn source_checkout_failed() -> Error {
    Error::new(
        ErrorCode::SourceCheckoutFailed,
        "Repro It could not read the Git source revision.",
    )
}

fn source_too_large() -> Error {
    Error::new(
        ErrorCode::RuntimeQuota,
        "The replay-host source closure exceeds its configured limit.",
    )
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    use super::*;

    #[test]
    fn source_closure_uses_exact_clean_revision_bytes_and_modes() {
        let root = tempfile::tempdir().unwrap();
        initialize_repository(root.path());
        fs::create_dir(root.path().join("target")).unwrap();
        fs::write(root.path().join("target/ignored-secret"), b"ignored").unwrap();
        let revision = head(root.path());

        let files = collect_source(root.path(), &revision).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            [".gitignore", "a.rs", "empty", "src/z.rs"]
        );
        assert_eq!(
            URL_SAFE_NO_PAD.decode(&files[1].bytes).unwrap(),
            b"a".to_vec()
        );
        assert!(files[1].executable);
        assert!(files[2].bytes.is_empty());
        assert!(
            files
                .iter()
                .all(|file| file.path.split('/').all(|component| component != ".git"))
        );

        fs::write(root.path().join("src/z.rs"), b"changed").unwrap();
        let Err(error) = collect_source(root.path(), &revision) else {
            panic!("a changed tracked file must stop source packaging");
        };
        assert_eq!(error.code, ErrorCode::SourceRevisionMissing);
    }

    #[test]
    fn source_closure_rejects_untracked_links_and_wrong_revision() {
        let root = tempfile::tempdir().unwrap();
        initialize_repository(root.path());
        let revision = head(root.path());
        fs::write(root.path().join("untracked.txt"), b"untracked").unwrap();
        let Err(error) = collect_source(root.path(), &revision) else {
            panic!("an untracked file must stop source packaging");
        };
        assert_eq!(error.code, ErrorCode::SourceRevisionMissing);

        fs::remove_file(root.path().join("untracked.txt")).unwrap();
        let Err(error) = collect_source(root.path(), "0000000000000000000000000000000000000000")
        else {
            panic!("a different revision must stop source packaging");
        };
        assert_eq!(error.code, ErrorCode::SourceRevisionMissing);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("a.rs", root.path().join("link.rs")).unwrap();
            run_git(root.path(), &["add", "link.rs"]);
            run_git(root.path(), &["commit", "-m", "tracked link"]);
            let revision = head(root.path());
            assert!(collect_source(root.path(), &revision).is_err());
        }
    }

    #[test]
    fn source_closure_clears_repository_redirection() {
        let root = tempfile::tempdir().unwrap();
        let command = sanitized_git(root.path());
        let removed = command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<BTreeSet<_>>();
        for name in [
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CEILING_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_NAMESPACE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_SHALLOW_FILE",
            "GIT_WORK_TREE",
        ] {
            assert!(removed.contains(std::ffi::OsStr::new(name)));
        }
    }

    fn initialize_repository(root: &Path) {
        run_git(root, &["init", "--quiet"]);
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), b"target/\n").unwrap();
        fs::write(root.join("src/z.rs"), b"z").unwrap();
        fs::write(root.join("a.rs"), b"a").unwrap();
        fs::write(root.join("empty"), b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(root.join("a.rs"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        run_git(root, &["add", ".gitignore", "a.rs", "empty", "src/z.rs"]);
        run_git(root, &["commit", "-m", "fixture"]);
    }

    fn head(root: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn run_git(root: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .env("GIT_AUTHOR_NAME", "Repro It")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Repro It")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
}
