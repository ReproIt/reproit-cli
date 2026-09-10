use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Instant,
};

use reproit_core::{Error, ErrorCode, canonical, identity::Digest};
use reproit_experiments::{
    AuthoredExperimentDefinition, CancellationFlag, MAX_PATH_BYTES, MAX_PATH_SEGMENTS, Measurement,
    RunPhase, Verdict, native_authored_command, parse_authored_measurements,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::authored_process::{cancelled_error, command_error, elapsed_milliseconds, run_process};

const MAX_ENVIRONMENT_TEXT_BYTES: usize = 256;
const MAX_INPUT_FILES: usize = 65_536;
const MAX_OBJECT_BYTES: u64 = 262_144;
const STDERR_MEDIA_TYPE: &str = "application/vnd.reproit.authored-stderr.v1";
const STDOUT_MEDIA_TYPE: &str = "application/vnd.reproit.authored-stdout.v1";

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum AuthoredCapsuleFormat {
    #[serde(rename = "reproit.research-capsule.v1")]
    V1,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthoredCapsule {
    definition: Digest,
    format: AuthoredCapsuleFormat,
    repository_id: String,
}

impl AuthoredCapsule {
    pub(crate) fn new(definition: Digest, repository_id: String) -> Self {
        Self {
            definition,
            format: AuthoredCapsuleFormat::V1,
            repository_id,
        }
    }

    pub(crate) fn definition(&self) -> Digest {
        self.definition
    }

    pub(crate) fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub(crate) fn digest(&self) -> Result<Digest, Error> {
        canonical::digest(self)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEnvironment {
    architecture: String,
    logical_processors: u32,
    memory_bytes: Option<u64>,
    operating_system: String,
    processor: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputReference {
    digest: Digest,
    media_type: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredAttempt {
    elapsed_milliseconds: u64,
    measurements: Vec<Measurement>,
    phase: RunPhase,
    sequence: u16,
    stderr: OutputReference,
    stdout: OutputReference,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredSetupAttempt {
    elapsed_milliseconds: u64,
    sequence: u16,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredVerifiedInput {
    digest: Digest,
    id: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum InputDirectoryFormat {
    #[serde(rename = "reproit.input-directory.v1")]
    V1,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputDirectory {
    entries: Vec<InputDirectoryEntry>,
    format: InputDirectoryFormat,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputDirectoryEntry {
    digest: Digest,
    path: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum AuthoredEvidenceFormat {
    #[serde(rename = "reproit.research-evidence.v1")]
    V1,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthoredExecutionEvidence {
    attempts: Vec<AuthoredAttempt>,
    capsule: Digest,
    environment: NativeEnvironment,
    format: AuthoredEvidenceFormat,
    setup_attempts: Vec<AuthoredSetupAttempt>,
    verified_inputs: Vec<AuthoredVerifiedInput>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum AuthoredResultFormat {
    #[serde(rename = "reproit.research-result.v1")]
    V1,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthoredCriterionResult {
    criterion_id: String,
    expected_maximum: u64,
    expected_minimum: u64,
    observed_maximum: u64,
    observed_median: u64,
    observed_minimum: u64,
    verdict: Verdict,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthoredExperimentResult {
    capsule: Digest,
    criteria: Vec<AuthoredCriterionResult>,
    format: AuthoredResultFormat,
    verdict: Verdict,
}

impl AuthoredExperimentResult {
    pub(crate) fn criteria(&self) -> &[AuthoredCriterionResult] {
        &self.criteria
    }

    pub(crate) fn verdict(&self) -> Verdict {
        self.verdict
    }
}

impl AuthoredCriterionResult {
    pub(crate) fn criterion_id(&self) -> &str {
        &self.criterion_id
    }

    pub(crate) fn expected_maximum(&self) -> u64 {
        self.expected_maximum
    }

    pub(crate) fn expected_minimum(&self) -> u64 {
        self.expected_minimum
    }

    pub(crate) fn observed_maximum(&self) -> u64 {
        self.observed_maximum
    }

    pub(crate) fn observed_median(&self) -> u64 {
        self.observed_median
    }

    pub(crate) fn observed_minimum(&self) -> u64 {
        self.observed_minimum
    }
}

pub(crate) struct CompletedAuthoredExperiment {
    pub(crate) capsule: AuthoredCapsule,
    pub(crate) evidence: AuthoredExecutionEvidence,
    pub(crate) object_root: PathBuf,
    pub(crate) result: AuthoredExperimentResult,
}

pub(crate) enum AuthoredExecutionOutcome {
    Completed(Box<CompletedAuthoredExperiment>),
    InputsUnverified,
    InvalidMeasurements,
    MeasurementCommandFailed,
    SetupCommandFailed,
}

impl AuthoredExecutionOutcome {
    pub(crate) fn failure_error(&self) -> Option<Error> {
        match self {
            Self::InputsUnverified => Some(inputs_unverified()),
            Self::InvalidMeasurements => Some(measurements_invalid()),
            Self::MeasurementCommandFailed => Some(run_failed()),
            Self::SetupCommandFailed => Some(setup_failed()),
            Self::Completed(_) => None,
        }
    }
}

pub(crate) fn execute(
    repository_id: &str,
    definition: &AuthoredExperimentDefinition,
    definition_digest: Digest,
    staging_root: &Path,
    cancellation: &CancellationFlag,
) -> Result<AuthoredExecutionOutcome, Error> {
    definition.validate()?;
    let capsule = AuthoredCapsule::new(definition_digest, repository_id.to_owned());
    let capsule_digest = capsule.digest()?;
    let workspace = staging_root.join("workspace");
    fs::create_dir(&workspace).map_err(|_| storage_error())?;
    let object_root = staging_root.join("objects/sha256");
    fs::create_dir_all(&object_root).map_err(|_| storage_error())?;
    let environment = native_environment()?;
    let started = Instant::now();
    let mut setup_attempts = Vec::with_capacity(definition.setup.len());
    for (index, setup) in definition.setup.iter().enumerate() {
        let setup = native_authored_command(setup, std::env::consts::EXE_SUFFIX);
        let output = run_process(&workspace, &setup, definition, cancellation, started)?;
        if !output.status.success() || output.limit_reached {
            return Ok(AuthoredExecutionOutcome::SetupCommandFailed);
        }
        setup_attempts.push(AuthoredSetupAttempt {
            elapsed_milliseconds: output.elapsed_milliseconds,
            sequence: u16::try_from(index + 1).map_err(|_| Error::schema_invalid())?,
        });
    }
    let Some(verified_inputs) = verify_inputs(definition, &workspace, cancellation, started)?
    else {
        return Ok(AuthoredExecutionOutcome::InputsUnverified);
    };
    let criterion_ids = definition.criterion_ids();
    let mut attempts = Vec::new();
    for (phase, count) in [
        (RunPhase::Warmup, definition.run.warmup_runs),
        (RunPhase::Measured, definition.run.measured_runs),
    ] {
        for sequence in 1..=count {
            let command =
                native_authored_command(&definition.run.command, std::env::consts::EXE_SUFFIX);
            let output = run_process(&workspace, &command, definition, cancellation, started)?;
            if !output.status.success() || output.limit_reached {
                return Ok(AuthoredExecutionOutcome::MeasurementCommandFailed);
            }
            let Ok(measurements) = parse_authored_measurements(&output.stdout, &criterion_ids)
            else {
                return Ok(AuthoredExecutionOutcome::InvalidMeasurements);
            };
            attempts.push(AuthoredAttempt {
                elapsed_milliseconds: output.elapsed_milliseconds,
                measurements,
                phase,
                sequence,
                stderr: store_output(&object_root, &output.stderr, STDERR_MEDIA_TYPE)?,
                stdout: store_output(&object_root, &output.stdout, STDOUT_MEDIA_TYPE)?,
            });
        }
    }
    let evidence = AuthoredExecutionEvidence {
        attempts,
        capsule: capsule_digest,
        environment,
        format: AuthoredEvidenceFormat::V1,
        setup_attempts,
        verified_inputs,
    };
    let result = result_from_evidence(definition, capsule_digest, &evidence)?;
    Ok(AuthoredExecutionOutcome::Completed(Box::new(
        CompletedAuthoredExperiment {
            capsule,
            evidence,
            object_root: staging_root.join("objects"),
            result,
        },
    )))
}

pub(crate) fn verify(
    definition: &AuthoredExperimentDefinition,
    capsule: &AuthoredCapsule,
    evidence: &AuthoredExecutionEvidence,
    result: &AuthoredExperimentResult,
    object_root: &Path,
) -> Result<(), Error> {
    definition.validate()?;
    let capsule_digest = capsule.digest()?;
    if capsule.definition != canonical::digest(definition)?
        || evidence.capsule != capsule_digest
        || result.capsule != capsule_digest
    {
        return Err(Error::object_digest_mismatch());
    }
    let criterion_ids = definition.criterion_ids();
    for attempt in &evidence.attempts {
        let stdout = read_output(object_root, &attempt.stdout, STDOUT_MEDIA_TYPE)?;
        let _stderr = read_output(object_root, &attempt.stderr, STDERR_MEDIA_TYPE)?;
        let measurements = parse_authored_measurements(&stdout, &criterion_ids)
            .map_err(|_| Error::object_digest_mismatch())?;
        if measurements != attempt.measurements {
            return Err(Error::object_digest_mismatch());
        }
    }
    let expected = result_from_evidence(definition, capsule_digest, evidence)?;
    if &expected != result {
        return Err(Error::object_digest_mismatch());
    }
    Ok(())
}

pub(crate) fn evidence_objects_present(
    evidence: &AuthoredExecutionEvidence,
    object_root: &Path,
) -> Result<bool, Error> {
    for reference in evidence
        .attempts
        .iter()
        .flat_map(|attempt| [&attempt.stdout, &attempt.stderr])
    {
        let path = object_root
            .join("sha256")
            .join(digest_file_name(reference.digest)?);
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Ok(_) | Err(_) => return Err(Error::object_digest_mismatch()),
        }
    }
    Ok(true)
}

fn verify_inputs(
    definition: &AuthoredExperimentDefinition,
    workspace: &Path,
    cancellation: &CancellationFlag,
    operation_started: Instant,
) -> Result<Option<Vec<AuthoredVerifiedInput>>, Error> {
    let mut verified = Vec::with_capacity(definition.inputs.len());
    for input in &definition.inputs {
        if operation_stopped(definition, cancellation, operation_started)? {
            return Ok(None);
        }
        let Some(path) = resolve_input_path(workspace, Path::new(&input.path)) else {
            return Ok(None);
        };
        let Some(observed) = input_digest(&path, definition, cancellation, operation_started)?
        else {
            return Ok(None);
        };
        if observed != input.digest {
            return Ok(None);
        }
        verified.push(AuthoredVerifiedInput {
            digest: observed,
            id: input.id.clone(),
        });
    }
    Ok(Some(verified))
}

fn resolve_input_path(workspace: &Path, relative: &Path) -> Option<PathBuf> {
    let root = fs::symlink_metadata(workspace).ok()?;
    if !root.file_type().is_dir() {
        return None;
    }
    let mut current = workspace.to_owned();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            return None;
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).ok()?;
        if metadata.file_type().is_symlink()
            || (index + 1 < component_count && !metadata.file_type().is_dir())
        {
            return None;
        }
    }
    Some(current)
}

fn input_digest(
    path: &Path,
    definition: &AuthoredExperimentDefinition,
    cancellation: &CancellationFlag,
    operation_started: Instant,
) -> Result<Option<Digest>, Error> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() {
        return Ok(None);
    }
    if metadata.file_type().is_file() {
        return hash_input_file(path, definition, cancellation, operation_started)
            .map(|hashed| hashed.map(|(digest, _)| digest));
    }
    if !metadata.file_type().is_dir() {
        return Ok(None);
    }
    hash_input_directory(path, definition, cancellation, operation_started)
}

fn hash_input_directory(
    root: &Path,
    definition: &AuthoredExperimentDefinition,
    cancellation: &CancellationFlag,
    operation_started: Instant,
) -> Result<Option<Digest>, Error> {
    let mut directories = vec![(root.to_owned(), String::new())];
    let mut entries = Vec::new();
    while let Some((directory, prefix)) = directories.pop() {
        if operation_stopped(definition, cancellation, operation_started)? {
            return Ok(None);
        }
        let Ok(children) = fs::read_dir(directory) else {
            return Ok(None);
        };
        for child in children {
            let Ok(child) = child else {
                return Ok(None);
            };
            let Some(name) = child.file_name().to_str().map(str::to_owned) else {
                return Ok(None);
            };
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if !valid_input_entry_path(&relative) {
                return Ok(None);
            }
            let Ok(metadata) = fs::symlink_metadata(child.path()) else {
                return Ok(None);
            };
            if metadata.file_type().is_dir() {
                directories.push((child.path(), relative));
            } else if metadata.file_type().is_file() {
                if entries.len() >= MAX_INPUT_FILES {
                    return Ok(None);
                }
                let Some((digest, size_bytes)) =
                    hash_input_file(&child.path(), definition, cancellation, operation_started)?
                else {
                    return Ok(None);
                };
                entries.push(InputDirectoryEntry {
                    digest,
                    path: relative,
                    size_bytes,
                });
            } else {
                return Ok(None);
            }
        }
    }
    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    canonical::digest(&InputDirectory {
        entries,
        format: InputDirectoryFormat::V1,
    })
    .map(Some)
}

fn hash_input_file(
    path: &Path,
    definition: &AuthoredExperimentDefinition,
    cancellation: &CancellationFlag,
    operation_started: Instant,
) -> Result<Option<(Digest, u64)>, Error> {
    let Ok(mut file) = File::open(path) else {
        return Ok(None);
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) | Err(_) => return Ok(None),
    };
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_u64;
    let mut buffer = [0_u8; 8_192];
    loop {
        if operation_stopped(definition, cancellation, operation_started)? {
            return Ok(None);
        }
        let Ok(count) = file.read(&mut buffer) else {
            return Ok(None);
        };
        if count == 0 {
            break;
        }
        size_bytes = match size_bytes.checked_add(u64::try_from(count).unwrap_or(u64::MAX)) {
            Some(size_bytes) => size_bytes,
            None => return Ok(None),
        };
        hasher.update(&buffer[..count]);
    }
    if size_bytes != metadata.len() {
        return Ok(None);
    }
    Ok(Some((
        Digest::from_bytes(hasher.finalize().into()),
        size_bytes,
    )))
}

fn operation_stopped(
    definition: &AuthoredExperimentDefinition,
    cancellation: &CancellationFlag,
    operation_started: Instant,
) -> Result<bool, Error> {
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    Ok(elapsed_milliseconds(operation_started) >= definition.run.limits.max_total_milliseconds)
}

fn valid_input_entry_path(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES
        && path.split('/').count() <= MAX_PATH_SEGMENTS
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn result_from_evidence(
    definition: &AuthoredExperimentDefinition,
    capsule: Digest,
    evidence: &AuthoredExecutionEvidence,
) -> Result<AuthoredExperimentResult, Error> {
    validate_evidence_shape(definition, evidence)?;
    let mut criteria = Vec::with_capacity(definition.criteria.len());
    for (criterion_index, criterion) in definition.criteria.iter().enumerate() {
        let values = measured_values(
            &evidence.attempts,
            criterion_index,
            &criterion.id,
            definition.run.measured_runs,
        )?;
        let observed_minimum = *values.first().ok_or_else(Error::schema_invalid)?;
        let observed_maximum = *values.last().ok_or_else(Error::schema_invalid)?;
        let observed_median = median(&values)?;
        let verdict = if (criterion.expected.minimum..=criterion.expected.maximum)
            .contains(&observed_median)
        {
            Verdict::Pass
        } else {
            Verdict::Regression
        };
        criteria.push(AuthoredCriterionResult {
            criterion_id: criterion.id.clone(),
            expected_maximum: criterion.expected.maximum,
            expected_minimum: criterion.expected.minimum,
            observed_maximum,
            observed_median,
            observed_minimum,
            verdict,
        });
    }
    let verdict = if criteria
        .iter()
        .all(|criterion| criterion.verdict == Verdict::Pass)
    {
        Verdict::Pass
    } else {
        Verdict::Regression
    };
    Ok(AuthoredExperimentResult {
        capsule,
        criteria,
        format: AuthoredResultFormat::V1,
        verdict,
    })
}

fn validate_evidence_shape(
    definition: &AuthoredExperimentDefinition,
    evidence: &AuthoredExecutionEvidence,
) -> Result<(), Error> {
    if evidence.environment.architecture.is_empty()
        || evidence.environment.operating_system.is_empty()
        || evidence.environment.processor.is_empty()
        || evidence.environment.processor.len() > MAX_ENVIRONMENT_TEXT_BYTES
        || evidence.environment.logical_processors == 0
    {
        return Err(Error::object_digest_mismatch());
    }
    if evidence.verified_inputs.len() != definition.inputs.len()
        || evidence
            .verified_inputs
            .iter()
            .zip(&definition.inputs)
            .any(|(verified, declared)| {
                verified.id != declared.id || verified.digest != declared.digest
            })
    {
        return Err(Error::object_digest_mismatch());
    }
    if evidence.setup_attempts.len() != definition.setup.len() {
        return Err(Error::object_digest_mismatch());
    }
    for (index, attempt) in evidence.setup_attempts.iter().enumerate() {
        if usize::from(attempt.sequence) != index + 1
            || attempt.elapsed_milliseconds > definition.run.limits.max_run_milliseconds
        {
            return Err(Error::object_digest_mismatch());
        }
    }
    let mut index = 0_usize;
    for (phase, count) in [
        (RunPhase::Warmup, definition.run.warmup_runs),
        (RunPhase::Measured, definition.run.measured_runs),
    ] {
        for sequence in 1..=count {
            let attempt = evidence
                .attempts
                .get(index)
                .ok_or_else(Error::object_digest_mismatch)?;
            if attempt.phase != phase
                || attempt.sequence != sequence
                || attempt.elapsed_milliseconds > definition.run.limits.max_run_milliseconds
                || attempt.measurements.len() != definition.criteria.len()
                || attempt
                    .measurements
                    .iter()
                    .zip(&definition.criteria)
                    .any(|(measurement, criterion)| measurement.criterion_id != criterion.id)
            {
                return Err(Error::object_digest_mismatch());
            }
            index = index
                .checked_add(1)
                .ok_or_else(Error::object_digest_mismatch)?;
        }
    }
    if index != evidence.attempts.len() {
        return Err(Error::object_digest_mismatch());
    }
    Ok(())
}

fn measured_values(
    attempts: &[AuthoredAttempt],
    criterion_index: usize,
    criterion_id: &str,
    expected_count: u16,
) -> Result<Vec<u64>, Error> {
    let mut values = attempts
        .iter()
        .filter(|attempt| attempt.phase == RunPhase::Measured)
        .map(|attempt| {
            let measurement = attempt
                .measurements
                .get(criterion_index)
                .ok_or_else(Error::object_digest_mismatch)?;
            (measurement.criterion_id == criterion_id)
                .then_some(measurement.value)
                .ok_or_else(Error::object_digest_mismatch)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() != usize::from(expected_count) {
        return Err(Error::object_digest_mismatch());
    }
    values.sort_unstable();
    Ok(values)
}

fn median(values: &[u64]) -> Result<u64, Error> {
    values
        .get(values.len() / 2)
        .copied()
        .ok_or_else(Error::schema_invalid)
}

fn store_output(root: &Path, bytes: &[u8], media_type: &str) -> Result<OutputReference, Error> {
    let size_bytes = u64::try_from(bytes.len()).map_err(|_| storage_error())?;
    if size_bytes > MAX_OBJECT_BYTES {
        return Err(storage_error());
    }
    let digest = Digest::of(bytes);
    let path = root.join(digest_file_name(digest)?);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| storage_error())?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_bounded(&path, MAX_OBJECT_BYTES)?;
            if existing != bytes {
                return Err(Error::object_digest_mismatch());
            }
        }
        Err(_) => return Err(storage_error()),
    }
    Ok(OutputReference {
        digest,
        media_type: media_type.to_owned(),
        size_bytes,
    })
}

fn read_output(
    object_root: &Path,
    reference: &OutputReference,
    media_type: &str,
) -> Result<Vec<u8>, Error> {
    if reference.media_type != media_type || reference.size_bytes > MAX_OBJECT_BYTES {
        return Err(Error::object_digest_mismatch());
    }
    let bytes = read_bounded(
        &object_root
            .join("sha256")
            .join(digest_file_name(reference.digest)?),
        MAX_OBJECT_BYTES,
    )?;
    if u64::try_from(bytes.len()).ok() != Some(reference.size_bytes)
        || Digest::of(&bytes) != reference.digest
    {
        return Err(Error::object_digest_mismatch());
    }
    Ok(bytes)
}

fn read_bounded(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, Error> {
    let metadata = fs::symlink_metadata(path).map_err(|_| storage_error())?;
    if !metadata.file_type().is_file() || metadata.len() > maximum_bytes {
        return Err(storage_error());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    File::open(path)
        .and_then(|file| file.take(maximum_bytes + 1).read_to_end(&mut bytes))
        .map_err(|_| storage_error())?;
    Ok(bytes)
}

fn digest_file_name(digest: Digest) -> Result<String, Error> {
    digest
        .to_string()
        .strip_prefix("sha256:")
        .filter(|value| value.len() == 64)
        .map(str::to_owned)
        .ok_or_else(storage_error)
}

fn native_environment() -> Result<NativeEnvironment, Error> {
    let logical_processors = std::thread::available_parallelism()
        .map_or(1, |count| u32::try_from(count.get()).unwrap_or(u32::MAX));
    let processor = processor_name();
    if processor.is_empty()
        || processor.len() > MAX_ENVIRONMENT_TEXT_BYTES
        || processor.chars().any(char::is_control)
    {
        return Err(command_error());
    }
    Ok(NativeEnvironment {
        architecture: std::env::consts::ARCH.to_owned(),
        logical_processors,
        memory_bytes: memory_bytes(),
        operating_system: std::env::consts::OS.to_owned(),
        processor,
    })
}

#[cfg(target_os = "macos")]
fn processor_name() -> String {
    command_text("sysctl", &["-n", "machdep.cpu.brand_string"])
        .or_else(|| command_text("sysctl", &["-n", "hw.model"]))
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

#[cfg(target_os = "linux")]
fn processor_name() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                matches!(name.trim(), "model name" | "Model" | "Hardware")
                    .then(|| value.trim().to_owned())
            })
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

#[cfg(target_os = "windows")]
fn processor_name() -> String {
    std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn processor_name() -> String {
    std::env::consts::ARCH.to_owned()
}

#[cfg(target_os = "macos")]
fn memory_bytes() -> Option<u64> {
    command_text("sysctl", &["-n", "hw.memsize"])?.parse().ok()
}

#[cfg(target_os = "linux")]
fn memory_bytes() -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kibibytes = text.lines().find_map(|line| {
        let value = line.strip_prefix("MemTotal:")?.trim();
        value.strip_suffix("kB")?.trim().parse::<u64>().ok()
    })?;
    kibibytes.checked_mul(1_024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn memory_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn command_text(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    if !output.status.success() || !output.stderr.is_empty() || output.stdout.len() > 256 {
        return None;
    }
    let value = std::str::from_utf8(&output.stdout).ok()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn setup_failed() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "An authored setup command failed.",
    )
}

fn inputs_unverified() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The authored inputs could not be verified, so the result is UNKNOWN.",
    )
}

fn run_failed() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The authored measurement command failed.",
    )
}

fn measurements_invalid() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The authored measurement command returned invalid evidence.",
    )
}

fn storage_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The authored experiment evidence could not be stored.",
    )
}
