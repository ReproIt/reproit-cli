use std::{
    fs::File,
    io::Read as _,
    path::Path,
    process::{Command, Stdio},
};

use reproit_backend::config::RunSpec;
use reproit_core::{Error, ErrorCode};

use crate::capture_probe::wait_for_probe;

const GO_REBUILD_FLAG: &str = "-a";
const GO_TOOLEXEC_FLAG: &str = "-toolexec=reproit";
const MAX_GO_PROBE_BINARY_BYTES: u64 = 512 * 1_024 * 1_024;
const GO_PROBE_READ_BYTES: usize = 64 * 1_024;
const REQUIRED_SYMBOLS: [&[u8]; 4] = [
    b"reproit.dev/sdk-go/reproit.instrumentedSetenv",
    b"reproit.dev/sdk-go/reproit.instrumentedTimeNow",
    b"syscall.reproitOriginalSetenv",
    b"time.reproitOriginalNow",
];

pub(crate) fn verify(repository_root: &Path, run: &RunSpec) -> Result<(), Error> {
    let package = go_package(run)?;
    let temporary = tempfile::Builder::new()
        .prefix("reproit-go-capture-probe-")
        .tempdir()
        .map_err(|_| capture_probe_failed())?;
    let subject = temporary.path().join("subject");
    let mut command = Command::new(&run.program);
    command
        .args([
            "build",
            GO_REBUILD_FLAG,
            GO_TOOLEXEC_FLAG,
            "-o",
            subject.to_str().ok_or_else(capture_probe_failed)?,
            package,
        ])
        .current_dir(repository_root.join(&run.working_directory))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| capture_probe_failed())?;
    if !wait_for_probe(&mut child)?.success() || !binary_has_required_symbols(&subject)? {
        return Err(capture_probe_failed());
    }
    Ok(())
}

fn go_package(run: &RunSpec) -> Result<&str, Error> {
    match run.arguments.as_slice() {
        [command, rebuild, toolexec, package, ..]
            if command == "run"
                && rebuild == GO_REBUILD_FLAG
                && toolexec == GO_TOOLEXEC_FLAG
                && !package.is_empty()
                && !package.starts_with('-')
                && !package.chars().any(char::is_control) =>
        {
            Ok(package)
        }
        _ => Err(capture_probe_failed()),
    }
}

fn binary_has_required_symbols(path: &Path) -> Result<bool, Error> {
    let metadata = path.metadata().map_err(|_| capture_probe_failed())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_GO_PROBE_BINARY_BYTES {
        return Ok(false);
    }
    let mut file = File::open(path).map_err(|_| capture_probe_failed())?;
    let mut missing = REQUIRED_SYMBOLS.to_vec();
    let mut previous = Vec::new();
    let mut chunk = vec![0_u8; GO_PROBE_READ_BYTES].into_boxed_slice();
    loop {
        let count = file.read(&mut chunk).map_err(|_| capture_probe_failed())?;
        if count == 0 {
            return Ok(missing.is_empty());
        }
        previous.extend_from_slice(&chunk[..count]);
        missing.retain(|symbol| !previous.windows(symbol.len()).any(|value| value == *symbol));
        if missing.is_empty() {
            return Ok(true);
        }
        let overlap = REQUIRED_SYMBOLS
            .iter()
            .map(|value| value.len())
            .max()
            .unwrap_or(1)
            .saturating_sub(1);
        if previous.len() > overlap {
            previous.drain(..previous.len() - overlap);
        }
    }
}

fn capture_probe_failed() -> Error {
    Error::new(
        ErrorCode::UnsupportedCapabilitySet,
        "The Go application did not build with complete automatic capture instrumentation.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_accepts_only_the_normalized_direct_package_position() {
        let valid = run(&["run", "-a", "-toolexec=reproit", "./cmd/service", "--port"]);
        assert_eq!(go_package(&valid).unwrap(), "./cmd/service");
        for arguments in [
            vec!["run", "./cmd/service"],
            vec!["run", "-a", "-toolexec=reproit", "-tags"],
            vec!["build", "-a", "-toolexec=reproit", "./cmd/service"],
        ] {
            assert!(go_package(&run(&arguments)).is_err());
        }
    }

    #[test]
    fn binary_probe_finds_every_symbol_across_read_boundaries() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut bytes = vec![b'x'; GO_PROBE_READ_BYTES - 3];
        for symbol in REQUIRED_SYMBOLS {
            bytes.extend_from_slice(symbol);
            bytes.push(b'\n');
        }
        std::fs::write(file.path(), &bytes).unwrap();
        assert!(binary_has_required_symbols(file.path()).unwrap());

        bytes.truncate(bytes.len() - REQUIRED_SYMBOLS[3].len() - 1);
        std::fs::write(file.path(), bytes).unwrap();
        assert!(!binary_has_required_symbols(file.path()).unwrap());
    }

    fn run(arguments: &[&str]) -> RunSpec {
        RunSpec {
            arguments: arguments.iter().map(|value| (*value).to_owned()).collect(),
            program: "go".to_owned(),
            working_directory: ".".to_owned(),
        }
    }
}
