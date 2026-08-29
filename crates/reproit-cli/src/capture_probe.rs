use std::{
    io::{Read as _, sink},
    path::Path,
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    thread,
    time::Duration,
};

use reproit_backend::config::{BackendSdk, RunSpec};
use reproit_core::{Error, ErrorCode};

const CAPTURE_PROBE_ENVIRONMENT: &str = "REPROIT_INTERNAL_CAPTURE_PROBE";
const CAPTURE_PROBE_SDK_ENVIRONMENT: &str = "REPROIT_INTERNAL_CAPTURE_PROBE_SDK";
const CAPTURE_PROBE_FORMAT: &str = "reproit.capture-probe.v1";
const MAX_CAPTURE_PROBE_OUTPUT_BYTES: u64 = 4 * 1_024;
const MAX_CAPTURE_PROBE_POLLS: usize = 6_000;
const CAPTURE_PROBE_POLL_MILLISECONDS: u64 = 10;

pub(crate) fn verify(repository_root: &Path, sdk: BackendSdk, run: &RunSpec) -> Result<(), Error> {
    if sdk == BackendSdk::Go {
        return crate::go_capture_probe::verify(repository_root, run);
    }
    let sdk_name = probe_sdk_name(sdk)?;
    let nonce = capture_probe_nonce()?;
    let mut command = Command::new(&run.program);
    command
        .args(&run.arguments)
        .current_dir(repository_root.join(&run.working_directory))
        .env(CAPTURE_PROBE_ENVIRONMENT, &nonce)
        .env(CAPTURE_PROBE_SDK_ENVIRONMENT, sdk_name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| capture_probe_failed())?;
    let stdout = child.stdout.take().ok_or_else(capture_probe_failed)?;
    let output = thread::spawn(move || read_probe_output(stdout));
    let status = wait_for_probe(&mut child);
    let output = output
        .join()
        .map_err(|_| capture_probe_failed())?
        .map_err(|_| capture_probe_failed())?;
    let status = status?;
    if !status.success() {
        return Err(capture_probe_failed());
    }
    let expected = format!("{CAPTURE_PROBE_FORMAT}:{sdk_name}:{nonce}\n");
    if output != expected.as_bytes() {
        return Err(capture_probe_failed());
    }
    Ok(())
}

fn probe_sdk_name(sdk: BackendSdk) -> Result<&'static str, Error> {
    match sdk {
        BackendSdk::Dotnet => Ok("dotnet"),
        BackendSdk::Nodejs => Ok("nodejs"),
        BackendSdk::Python => Ok("python"),
        BackendSdk::Rust => Ok("rust"),
        BackendSdk::Go => Err(unsupported_capture()),
    }
}

fn capture_probe_nonce() -> Result<String, Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| capture_probe_failed())?;
    Ok(hex::encode(bytes))
}

pub(crate) fn wait_for_probe(child: &mut Child) -> Result<ExitStatus, Error> {
    for _ in 0..MAX_CAPTURE_PROBE_POLLS {
        if let Some(status) = child.try_wait().map_err(|_| capture_probe_failed())? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(CAPTURE_PROBE_POLL_MILLISECONDS));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(capture_probe_failed())
}

fn read_probe_output(mut output: ChildStdout) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    output
        .by_ref()
        .take(MAX_CAPTURE_PROBE_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    std::io::copy(&mut output, &mut sink())?;
    Ok(bytes)
}

fn unsupported_capture() -> Error {
    Error::new(
        ErrorCode::UnsupportedCapabilitySet,
        "The selected SDK release does not support complete automatic World capture.",
    )
}

fn capture_probe_failed() -> Error {
    Error::new(
        ErrorCode::UnsupportedCapabilitySet,
        "The application did not load complete automatic World capture support.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_direct_runtime_startup_path_has_a_probe() {
        assert_eq!(probe_sdk_name(BackendSdk::Dotnet).unwrap(), "dotnet");
        assert_eq!(probe_sdk_name(BackendSdk::Nodejs).unwrap(), "nodejs");
        assert_eq!(probe_sdk_name(BackendSdk::Python).unwrap(), "python");
        assert!(probe_sdk_name(BackendSdk::Go).is_err());
        assert_eq!(probe_sdk_name(BackendSdk::Rust).unwrap(), "rust");
    }

    #[test]
    fn nonce_has_the_exact_shared_shape() {
        let nonce = capture_probe_nonce().unwrap();
        assert_eq!(nonce.len(), 64);
        assert!(
            nonce
                .bytes()
                .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
        );
    }
}
