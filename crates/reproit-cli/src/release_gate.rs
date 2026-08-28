use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reproit_core::{Error, ErrorCode, canonical};
use reproit_experiments::ReleaseDecision;
use reproit_ml::{
    CommandExecutionOutcome, CommandModelRunner, CommandRunLimits, CommandRunResult, CommandSpec,
    EvaluationSuite, EvaluationVerdict, ModelIdentity, ModelRun, Observation,
    ObservationUnavailableReason, VerdictStatus, evaluate,
};
use serde::{Deserialize, Serialize};

const MAX_CONFIG_BYTES: u64 = 1_048_576;
const MAX_INPUT_FILE_BYTES: u64 = 67_108_864;
const MAX_BUNDLE_BYTES: u64 = 234_881_024;
const BUNDLE_FORMAT: &str = "reproit.release-evidence-bundle.v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateConfig {
    format: GateConfigFormat,
    suite_path: PathBuf,
    bundle_path: PathBuf,
    limits: GateLimits,
    baseline: WorkloadConfig,
    candidate: WorkloadConfig,
}

#[derive(Debug, Deserialize)]
enum GateConfigFormat {
    #[serde(rename = "reproit.release-gate-config.v1")]
    V1,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateLimits {
    #[serde(rename = "max_execution_seconds")]
    execution_seconds: u64,
    #[serde(rename = "max_records")]
    records: usize,
    #[serde(rename = "max_stderr_bytes")]
    stderr_bytes: usize,
    #[serde(rename = "max_stdin_bytes")]
    stdin_bytes: usize,
    #[serde(rename = "max_stdout_bytes")]
    stdout_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkloadConfig {
    executable: String,
    arguments: Vec<String>,
    model_path: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceBundle {
    content: BundleContent,
    content_digest: String,
    format: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleContent {
    baseline: CommandEvidence,
    bindings: DigestBindings,
    candidate: CommandEvidence,
    decision: ReleaseDecision,
    suite: EvaluationSuite,
    verdict: EvaluationVerdict,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandEvidence {
    invocation: CommandInvocation,
    model_run: ModelRun,
    outcome: ExecutionOutcome,
    stderr_base64: String,
    stderr_digest: String,
    stdout_base64: String,
    stdout_digest: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandInvocation {
    arguments: Vec<String>,
    executable: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ExecutionOutcome {
    InvalidOutput,
    NonZeroExit { exit_code: Option<i32> },
    ObservationLimitExceeded,
    StderrLimitExceeded,
    StderrReadFailed,
    StdinWriteFailed,
    StdoutLimitExceeded,
    StdoutReadFailed,
    Succeeded,
    TimedOut,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DigestBindings {
    baseline_model_run: String,
    baseline_stderr: String,
    baseline_stdout: String,
    candidate_model_run: String,
    candidate_stderr: String,
    candidate_stdout: String,
    suite: String,
    verdict: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputRecord {
    case_id: String,
    output_text: String,
}

pub fn run(config_path: &Path) -> Result<ReleaseDecision, Error> {
    let config_bytes = read_regular_file(config_path, MAX_CONFIG_BYTES, config_read_error)?;
    let config: GateConfig =
        toml::from_str(std::str::from_utf8(&config_bytes).map_err(|_| config_invalid())?)
            .map_err(|_| config_invalid())?;
    let GateConfigFormat::V1 = config.format;
    validate_relative_input(&config.suite_path)?;
    validate_relative_input(&config.baseline.model_path)?;
    validate_relative_input(&config.candidate.model_path)?;
    validate_relative_output(&config.bundle_path)?;

    let config_root = config_path.parent().unwrap_or_else(|| Path::new("."));
    let suite: EvaluationSuite = read_json(&config_root.join(&config.suite_path))?;
    let baseline_model: ModelIdentity = read_json(&config_root.join(&config.baseline.model_path))?;
    let candidate_model: ModelIdentity =
        read_json(&config_root.join(&config.candidate.model_path))?;
    let runner = CommandModelRunner::new(config.limits.into()).map_err(|_| limits_invalid())?;

    let baseline = run_workload(
        &runner,
        &suite,
        baseline_model,
        "baseline",
        &config.baseline,
        config_root,
    )?;
    let candidate = run_workload(
        &runner,
        &suite,
        candidate_model,
        "candidate",
        &config.candidate,
        config_root,
    )?;
    let verdict = evaluate(&suite, &baseline.model_run, &candidate.model_run)
        .map_err(|_| evaluation_failed())?;
    let decision = decision_for(verdict.status);
    let content = make_content(suite, baseline, candidate, verdict, decision)?;
    let bundle = EvidenceBundle {
        content_digest: canonical_digest(&content)?,
        content,
        format: BUNDLE_FORMAT.to_owned(),
    };
    write_bundle(&config_root.join(config.bundle_path), &bundle)?;
    Ok(decision)
}

pub fn verify(bundle_path: &Path) -> Result<ReleaseDecision, Error> {
    let bytes = read_regular_file(bundle_path, MAX_BUNDLE_BYTES, bundle_read_error)?;
    let bundle: EvidenceBundle = serde_json::from_slice(&bytes).map_err(|_| bundle_invalid())?;
    if bundle.format != BUNDLE_FORMAT || bundle.content_digest != canonical_digest(&bundle.content)?
    {
        return Err(bundle_mismatch());
    }
    verify_bindings(&bundle.content)?;
    verify_command_evidence(&bundle.content.suite, &bundle.content.baseline)?;
    verify_command_evidence(&bundle.content.suite, &bundle.content.candidate)?;
    let verdict = evaluate(
        &bundle.content.suite,
        &bundle.content.baseline.model_run,
        &bundle.content.candidate.model_run,
    )
    .map_err(|_| bundle_mismatch())?;
    let decision = decision_for(verdict.status);
    if verdict != bundle.content.verdict || decision != bundle.content.decision {
        return Err(bundle_mismatch());
    }
    Ok(decision)
}

fn run_workload(
    runner: &CommandModelRunner,
    suite: &EvaluationSuite,
    model: ModelIdentity,
    role: &str,
    config: &WorkloadConfig,
    config_root: &Path,
) -> Result<CommandEvidence, Error> {
    if config.executable.is_empty() {
        return Err(config_invalid());
    }
    let executable = resolve_executable(config_root, &config.executable);
    let invocation = CommandInvocation {
        arguments: config.arguments.clone(),
        executable: executable.to_string_lossy().into_owned(),
    };
    let command = CommandSpec::new(
        executable,
        config.arguments.iter().map(OsString::from).collect(),
    );
    let result = runner
        .run(suite, model, random_run_id(role)?, &command)
        .map_err(|_| command_failed())?;
    Ok(command_evidence(invocation, result))
}

fn command_evidence(invocation: CommandInvocation, result: CommandRunResult) -> CommandEvidence {
    CommandEvidence {
        invocation,
        model_run: result.model_run,
        outcome: result.outcome.into(),
        stderr_base64: URL_SAFE_NO_PAD.encode(&result.stderr),
        stderr_digest: digest_bytes(&result.stderr),
        stdout_base64: URL_SAFE_NO_PAD.encode(&result.stdout),
        stdout_digest: digest_bytes(&result.stdout),
    }
}

fn make_content(
    suite: EvaluationSuite,
    baseline: CommandEvidence,
    candidate: CommandEvidence,
    verdict: EvaluationVerdict,
    decision: ReleaseDecision,
) -> Result<BundleContent, Error> {
    let bindings = DigestBindings {
        baseline_model_run: canonical_digest(&baseline.model_run)?,
        baseline_stderr: baseline.stderr_digest.clone(),
        baseline_stdout: baseline.stdout_digest.clone(),
        candidate_model_run: canonical_digest(&candidate.model_run)?,
        candidate_stderr: candidate.stderr_digest.clone(),
        candidate_stdout: candidate.stdout_digest.clone(),
        suite: canonical_digest(&suite)?,
        verdict: canonical_digest(&verdict)?,
    };
    Ok(BundleContent {
        baseline,
        bindings,
        candidate,
        decision,
        suite,
        verdict,
    })
}

fn verify_bindings(content: &BundleContent) -> Result<(), Error> {
    let expected = DigestBindings {
        baseline_model_run: canonical_digest(&content.baseline.model_run)?,
        baseline_stderr: content.baseline.stderr_digest.clone(),
        baseline_stdout: content.baseline.stdout_digest.clone(),
        candidate_model_run: canonical_digest(&content.candidate.model_run)?,
        candidate_stderr: content.candidate.stderr_digest.clone(),
        candidate_stdout: content.candidate.stdout_digest.clone(),
        suite: canonical_digest(&content.suite)?,
        verdict: canonical_digest(&content.verdict)?,
    };
    let verdict_links_match = content.bindings.baseline_model_run
        == serialized_digest(&content.verdict.baseline_run_digest)?
        && content.bindings.candidate_model_run
            == serialized_digest(&content.verdict.candidate_run_digest)?
        && content.bindings.suite == serialized_digest(&content.verdict.suite_digest)?;
    if content.bindings != expected || !verdict_links_match {
        return Err(bundle_mismatch());
    }
    Ok(())
}

fn verify_command_evidence(
    suite: &EvaluationSuite,
    evidence: &CommandEvidence,
) -> Result<(), Error> {
    let stdout = decode_raw(&evidence.stdout_base64)?;
    let stderr = decode_raw(&evidence.stderr_base64)?;
    if digest_bytes(&stdout) != evidence.stdout_digest
        || digest_bytes(&stderr) != evidence.stderr_digest
    {
        return Err(bundle_mismatch());
    }
    match evidence.outcome.unavailable_reason() {
        None => verify_successful_output(suite, &evidence.model_run, &stdout),
        Some(reason) => verify_unavailable_output(suite, &evidence.model_run, reason),
    }
}

fn verify_successful_output(
    suite: &EvaluationSuite,
    model_run: &ModelRun,
    stdout: &[u8],
) -> Result<(), Error> {
    let text = std::str::from_utf8(stdout).map_err(|_| bundle_mismatch())?;
    let mut outputs = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() || outputs.len() >= suite.cases.len() {
            return Err(bundle_mismatch());
        }
        let record: OutputRecord = serde_json::from_str(line).map_err(|_| bundle_mismatch())?;
        if outputs.insert(record.case_id, record.output_text).is_some() {
            return Err(bundle_mismatch());
        }
    }
    if model_run.observations.len() != suite.cases.len() {
        return Err(bundle_mismatch());
    }
    for (case, observation) in suite.cases.iter().zip(&model_run.observations) {
        match (outputs.get(&case.case_id), observation) {
            (
                Some(output),
                Observation::Complete {
                    case_id,
                    output_text,
                    ..
                },
            ) if case_id == &case.case_id && output_text == output => {}
            (
                None,
                Observation::Unavailable {
                    case_id,
                    reason: ObservationUnavailableReason::OutputMissing,
                },
            ) if case_id == &case.case_id => {}
            _ => return Err(bundle_mismatch()),
        }
    }
    Ok(())
}

fn verify_unavailable_output(
    suite: &EvaluationSuite,
    model_run: &ModelRun,
    expected_reason: ObservationUnavailableReason,
) -> Result<(), Error> {
    if model_run.observations.len() == suite.cases.len()
        && suite
            .cases
            .iter()
            .zip(&model_run.observations)
            .all(|(case, observation)| {
                matches!(
                    observation,
                    Observation::Unavailable { case_id, reason }
                        if case_id == &case.case_id && *reason == expected_reason
                )
            })
    {
        Ok(())
    } else {
        Err(bundle_mismatch())
    }
}

fn decode_raw(value: &str) -> Result<Vec<u8>, Error> {
    URL_SAFE_NO_PAD.decode(value).map_err(|_| bundle_mismatch())
}

fn decision_for(status: VerdictStatus) -> ReleaseDecision {
    match status {
        VerdictStatus::Pass => ReleaseDecision::Pass,
        VerdictStatus::Regression => ReleaseDecision::Regression,
        VerdictStatus::Unknown => ReleaseDecision::Unknown,
    }
}

fn resolve_executable(config_root: &Path, executable: &str) -> PathBuf {
    let path = Path::new(executable);
    if path.components().count() > 1 && path.is_relative() {
        config_root.join(path)
    } else {
        path.to_owned()
    }
}

fn validate_relative_input(path: &Path) -> Result<(), Error> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(config_invalid());
    }
    Ok(())
}

fn validate_relative_output(path: &Path) -> Result<(), Error> {
    validate_relative_input(path)?;
    if path.file_name().is_none() {
        return Err(config_invalid());
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, Error> {
    let bytes = read_regular_file(path, MAX_INPUT_FILE_BYTES, input_read_error)?;
    serde_json::from_slice(&bytes).map_err(|_| input_invalid())
}

fn read_regular_file(path: &Path, max_bytes: u64, error: fn() -> Error) -> Result<Vec<u8>, Error> {
    let metadata = fs::symlink_metadata(path).map_err(|_| error())?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return Err(error());
    }
    let bytes = fs::read(path).map_err(|_| error())?;
    if bytes.len() as u64 > max_bytes {
        return Err(error());
    }
    Ok(bytes)
}

fn write_bundle(path: &Path, bundle: &EvidenceBundle) -> Result<(), Error> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_metadata = fs::metadata(parent).map_err(|_| bundle_write_error())?;
    if !parent_metadata.is_dir() {
        return Err(bundle_write_error());
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err(bundle_write_error());
    }
    let bytes = serde_json::to_vec_pretty(bundle).map_err(|_| bundle_write_error())?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(bundle_write_error());
    }
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| bundle_write_error())?;
    temporary
        .write_all(&bytes)
        .map_err(|_| bundle_write_error())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| bundle_write_error())?;
    temporary.persist(path).map_err(|_| bundle_write_error())?;
    Ok(())
}

fn random_run_id(role: &str) -> Result<String, Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| command_failed())?;
    Ok(format!("{role}-{}", hex::encode(bytes)))
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<String, Error> {
    canonical::digest(value)
        .map(|digest| digest.to_string())
        .map_err(|_| bundle_invalid())
}

fn serialized_digest<T: Serialize>(value: &T) -> Result<String, Error> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(bundle_invalid)
}

fn digest_bytes(value: &[u8]) -> String {
    reproit_core::identity::Digest::of(value).to_string()
}

impl From<GateLimits> for CommandRunLimits {
    fn from(value: GateLimits) -> Self {
        Self {
            max_execution_time: Duration::from_secs(value.execution_seconds),
            max_records: value.records,
            max_stderr_bytes: value.stderr_bytes,
            max_stdin_bytes: value.stdin_bytes,
            max_stdout_bytes: value.stdout_bytes,
        }
    }
}

impl From<CommandExecutionOutcome> for ExecutionOutcome {
    fn from(value: CommandExecutionOutcome) -> Self {
        match value {
            CommandExecutionOutcome::InvalidOutput => Self::InvalidOutput,
            CommandExecutionOutcome::NonZeroExit { exit_code } => Self::NonZeroExit { exit_code },
            CommandExecutionOutcome::ObservationLimitExceeded => Self::ObservationLimitExceeded,
            CommandExecutionOutcome::StderrLimitExceeded => Self::StderrLimitExceeded,
            CommandExecutionOutcome::StderrReadFailed => Self::StderrReadFailed,
            CommandExecutionOutcome::StdinWriteFailed => Self::StdinWriteFailed,
            CommandExecutionOutcome::StdoutLimitExceeded => Self::StdoutLimitExceeded,
            CommandExecutionOutcome::StdoutReadFailed => Self::StdoutReadFailed,
            CommandExecutionOutcome::Succeeded => Self::Succeeded,
            CommandExecutionOutcome::TimedOut => Self::TimedOut,
        }
    }
}

impl ExecutionOutcome {
    const fn unavailable_reason(self) -> Option<ObservationUnavailableReason> {
        match self {
            Self::Succeeded => None,
            Self::InvalidOutput => Some(ObservationUnavailableReason::ProtocolInvalid),
            Self::ObservationLimitExceeded | Self::StdoutLimitExceeded => {
                Some(ObservationUnavailableReason::OutputTooLarge)
            }
            Self::StderrLimitExceeded => Some(ObservationUnavailableReason::LogTooLarge),
            Self::TimedOut => Some(ObservationUnavailableReason::ExecutionTimedOut),
            Self::NonZeroExit { .. }
            | Self::StderrReadFailed
            | Self::StdinWriteFailed
            | Self::StdoutReadFailed => Some(ObservationUnavailableReason::ExecutionFailed),
        }
    }
}

fn config_invalid() -> Error {
    Error::new(
        ErrorCode::SchemaInvalid,
        "The release-gate configuration is invalid.",
    )
}

fn config_read_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not read the release-gate configuration.",
    )
}

fn limits_invalid() -> Error {
    Error::new(
        ErrorCode::RuntimeQuota,
        "The release-gate limits are invalid.",
    )
}

fn input_read_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not read a release-gate input.",
    )
}

fn input_invalid() -> Error {
    Error::new(ErrorCode::SchemaInvalid, "A release-gate input is invalid.")
}

fn command_failed() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not run an evaluation command.",
    )
}

fn evaluation_failed() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not evaluate the candidate.",
    )
}

fn bundle_read_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not read the evidence bundle.",
    )
}

fn bundle_invalid() -> Error {
    Error::new(ErrorCode::SchemaInvalid, "The evidence bundle is invalid.")
}

fn bundle_mismatch() -> Error {
    Error::new(
        ErrorCode::ObjectDigestMismatch,
        "The evidence bundle failed verification.",
    )
}

fn bundle_write_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not write the evidence bundle.",
    )
}
