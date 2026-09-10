use reproit_core::{Error, ErrorCode};
use reproit_experiments::{AuthoredCommand, AuthoredExperimentDefinition, CancellationFlag};
use reproit_sdk_platform::process::{CLEANUP_TIMEOUT, ProcessTree, finish_reader};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Component, Path},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) struct ProcessOutput {
    pub(super) elapsed_milliseconds: u64,
    pub(super) limit_reached: bool,
    pub(super) status: ExitStatus,
    pub(super) stderr: Vec<u8>,
    pub(super) stdout: Vec<u8>,
}

pub(super) fn run_process(
    source_root: &Path,
    command: &AuthoredCommand,
    definition: &AuthoredExperimentDefinition,
    cancellation: &CancellationFlag,
    operation_started: Instant,
) -> Result<ProcessOutput, Error> {
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    let working_directory = source_root.join(&command.working_directory);
    let metadata = fs::symlink_metadata(&working_directory).map_err(|_| command_error())?;
    if !metadata.file_type().is_dir() {
        return Err(command_error());
    }
    let program = resolve_program(&working_directory, &command.program)?;
    let started = Instant::now();
    let mut process = Command::new(program);
    process
        .args(&command.arguments)
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ProcessTree::spawn(process).map_err(|_| command_error())?;
    let stdout = child.take_stdout().ok_or_else(command_error)?;
    let stderr = child.take_stderr().ok_or_else(command_error)?;
    let output_limit = Arc::new(AtomicBool::new(false));
    let stdout_reader = bounded_reader(
        stdout,
        definition.run.limits.max_stdout_bytes,
        Arc::clone(&output_limit),
    );
    let stderr_reader = bounded_reader(
        stderr,
        definition.run.limits.max_stderr_bytes,
        Arc::clone(&output_limit),
    );
    let status = wait_for_process(
        &mut child,
        definition,
        cancellation,
        operation_started,
        started,
        &output_limit,
    );
    let cleanup = child.terminate().map_err(|_| command_error());
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    let stdout = join_reader(stdout_reader, deadline);
    let stderr = join_reader(stderr_reader, deadline);
    cleanup?;
    let stdout = stdout?;
    let stderr = stderr?;
    let status = status?;
    Ok(ProcessOutput {
        elapsed_milliseconds: elapsed_milliseconds(started),
        limit_reached: output_limit.load(Ordering::Acquire),
        status,
        stderr,
        stdout,
    })
}

fn wait_for_process(
    child: &mut ProcessTree,
    definition: &AuthoredExperimentDefinition,
    cancellation: &CancellationFlag,
    operation_started: Instant,
    process_started: Instant,
    output_limit: &AtomicBool,
) -> Result<ExitStatus, Error> {
    loop {
        if let Some(status) = child.try_wait().map_err(|_| command_error())? {
            return Ok(status);
        }
        let run_elapsed = elapsed_milliseconds(process_started);
        let total_elapsed = elapsed_milliseconds(operation_started);
        if cancellation.is_cancelled()
            || output_limit.load(Ordering::Acquire)
            || run_elapsed >= definition.run.limits.max_run_milliseconds
            || total_elapsed >= definition.run.limits.max_total_milliseconds
        {
            let status = child.terminate().map_err(|_| command_error())?;
            if cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            return Ok(status);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn bounded_reader(
    mut reader: impl Read + Send + 'static,
    maximum_bytes: u64,
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
            let next = u64::try_from(kept.len())
                .unwrap_or(u64::MAX)
                .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            if next > maximum_bytes {
                exceeded.store(true, Ordering::Release);
                return Ok(kept);
            }
            kept.extend_from_slice(&buffer[..count]);
        }
    })
}

fn join_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    deadline: Instant,
) -> Result<Vec<u8>, Error> {
    finish_reader(reader, deadline)
        .map_err(|_| command_error())?
        .map_err(|_| command_error())
}

fn resolve_program(working_directory: &Path, program: &str) -> Result<OsString, Error> {
    let path = Path::new(program);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(Error::schema_invalid());
    }
    if program.contains('/') || program.contains('\\') {
        Ok(working_directory.join(path).into_os_string())
    } else {
        Ok(OsString::from(program))
    }
}

pub(super) fn elapsed_milliseconds(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn cancelled_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The authored experiment was cancelled.",
    )
}

pub(super) fn command_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The authored experiment command could not run.",
    )
}
