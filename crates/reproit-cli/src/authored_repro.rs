use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    str::FromStr as _,
};

use reproit_app::profiles::{AUTHORED_REPRO_CAPABILITY, ProfileCapabilityRegistry};
use reproit_core::{
    Error, ErrorCode, canonical,
    identity::{Digest, ReproId},
};
use reproit_experiments::{
    AuthoredClaim, AuthoredExperimentDefinition, CancellationFlag, Verdict, verify_authored_claim,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    authored_execution::{
        AuthoredCapsule, AuthoredExecutionEvidence, AuthoredExecutionOutcome,
        AuthoredExperimentResult, CompletedAuthoredExperiment, evidence_objects_present, execute,
        verify,
    },
    current_git_repository,
};

const MAX_DEFINITION_BYTES: u64 = 1_048_576;
const MAX_STORED_JSON_BYTES: u64 = 536_870_912;
const MAX_AUTHORED_REPROS: usize = 1_000;
const MAX_OBJECTS: usize = 1_024;
const PROFILE: &str = "experiments";

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddReproInput {
    pub definition_path: String,
}

impl AddReproInput {
    pub fn validate(&self) -> Result<(), Error> {
        validate_relative_path(Path::new(&self.definition_path))
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddReproResult {
    pub capsule_digest: Digest,
    pub claim_digest: Digest,
    pub next_command: String,
    pub observed_results: Vec<AuthoredObservedResult>,
    pub profile: String,
    pub repro_id: ReproId,
    pub tracked_reference_path: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredObservedResult {
    pub criterion_id: String,
    pub expected_maximum: u64,
    pub expected_minimum: u64,
    pub observed_maximum: u64,
    pub observed_median: u64,
    pub observed_minimum: u64,
    pub unit: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AuthoredCheckOutcome {
    Pass,
    Regression,
    Unknown,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AuthoredCheckResult {
    pub observed_results: Vec<AuthoredObservedResult>,
    pub outcome: AuthoredCheckOutcome,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum AuthoredReproReferenceFormat {
    #[serde(rename = "reproit.authored-repro-reference.v1")]
    V1,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredReproReference {
    capsule_digest: Digest,
    claim_digest: Digest,
    definition_digest: Digest,
    evidence_digest: Digest,
    format: AuthoredReproReferenceFormat,
    profile: String,
    repro_id: ReproId,
    repository_id: String,
    result_digest: Digest,
}

enum OpenedAuthoredRepro {
    MissingEvidence,
    Ready {
        capsule: AuthoredCapsule,
        definition: Box<AuthoredExperimentDefinition>,
    },
}

pub fn add_repro(root: &Path, input: &AddReproInput) -> Result<AddReproResult, Error> {
    add_repro_with_cancellation(root, input, &CancellationFlag::default())
}

pub fn add_repro_with_cancellation(
    root: &Path,
    input: &AddReproInput,
    cancellation: &CancellationFlag,
) -> Result<AddReproResult, Error> {
    input.validate()?;
    let config_root = require_initialized(root)?;
    let definition_path = resolve_regular_file(root, Path::new(&input.definition_path))?;
    let definition_bytes = read_bounded(&definition_path, MAX_DEFINITION_BYTES)?;
    let authored: AuthoredExperimentDefinition = canonical::parse_strict(&definition_bytes)?;
    ProfileCapabilityRegistry::installed().require(&authored.profile, AUTHORED_REPRO_CAPABILITY)?;
    authored.validate()?;

    let canonical_definition = canonical::canonical_bytes(&authored)?;
    let definition_digest = Digest::of(&canonical_definition);
    let canonical_root = fs::canonicalize(root).map_err(|_| source_error())?;
    let repository = current_git_repository(&canonical_root)?;
    if repository.root != canonical_root {
        return Err(source_error());
    }
    let staging = tempfile::Builder::new()
        .prefix("authored-repro-")
        .tempdir_in(&config_root)
        .map_err(|_| storage_error())?;
    let outcome = execute(
        &repository.repository_id,
        &authored,
        definition_digest,
        staging.path(),
        cancellation,
    )?;
    let AuthoredExecutionOutcome::Completed(completed) = outcome else {
        return Err(outcome
            .failure_error()
            .expect("non-completed authored execution outcome"));
    };
    verify_completed(&authored, &completed)?;
    if completed.result.verdict() != Verdict::Pass {
        return Err(range_failed());
    }

    let capsule_digest = completed.capsule.digest()?;
    let result_digest = canonical::digest(&completed.result)?;
    let claim = AuthoredClaim::new(&authored, capsule_digest, result_digest)?;
    verify_authored_claim(&claim, &authored, capsule_digest, result_digest)?;
    let claim_digest = claim.digest()?;
    let repro_id = derived_repro_id(capsule_digest)?;
    let reference = AuthoredReproReference {
        capsule_digest,
        claim_digest,
        definition_digest,
        evidence_digest: canonical::digest(&completed.evidence)?,
        format: AuthoredReproReferenceFormat::V1,
        profile: PROFILE.to_owned(),
        repro_id,
        repository_id: repository.repository_id,
        result_digest,
    };
    let sealed = staging.path().join("sealed");
    fs::create_dir(&sealed).map_err(|_| archive_error())?;
    let objects = sealed.join("objects");
    copy_objects(&completed.object_root, &objects).map_err(|_| archive_error())?;
    write_canonical_new(&sealed.join("definition.json"), &authored).map_err(|_| archive_error())?;
    write_canonical_new(&sealed.join("capsule.json"), &completed.capsule)
        .map_err(|_| archive_error())?;
    write_canonical_new(&sealed.join("claim.json"), &claim).map_err(|_| archive_error())?;
    write_canonical_new(&sealed.join("initial-evidence.json"), &completed.evidence)
        .map_err(|_| archive_error())?;
    write_canonical_new(&sealed.join("initial-result.json"), &completed.result)
        .map_err(|_| archive_error())?;
    write_canonical_new(&sealed.join("reference.json"), &reference).map_err(|_| archive_error())?;
    publish_sealed(&config_root, repro_id, &sealed).map_err(|_| publish_error())?;

    let observed_results = observed_results(&completed.result, &authored);

    Ok(AddReproResult {
        capsule_digest,
        claim_digest,
        next_command: format!("reproit check {repro_id}"),
        observed_results,
        profile: PROFILE.to_owned(),
        repro_id,
        tracked_reference_path: reference_path(repro_id),
    })
}

pub fn check_authored_repro(
    root: &Path,
    repro_id: ReproId,
) -> Result<Option<AuthoredCheckResult>, Error> {
    let Some(opened) = open_authored_repro(root, repro_id)? else {
        return Ok(None);
    };
    let OpenedAuthoredRepro::Ready {
        capsule,
        definition,
    } = opened
    else {
        return Ok(Some(unknown_check_result()));
    };
    let config_root = require_initialized(root)?;
    let canonical_root = fs::canonicalize(root).map_err(|_| source_error())?;
    let repository = current_git_repository(&canonical_root)?;
    if repository.root != canonical_root || repository.repository_id != capsule.repository_id() {
        return Err(source_error());
    }
    let staging = tempfile::Builder::new()
        .prefix("authored-check-")
        .tempdir_in(config_root)
        .map_err(|_| storage_error())?;
    let outcome = execute(
        &repository.repository_id,
        &definition,
        capsule.definition(),
        staging.path(),
        &CancellationFlag::default(),
    )?;
    let AuthoredExecutionOutcome::Completed(completed) = outcome else {
        return Ok(Some(AuthoredCheckResult {
            observed_results: Vec::new(),
            outcome: AuthoredCheckOutcome::Unknown,
        }));
    };
    verify_completed(&definition, &completed)?;
    let outcome = match completed.result.verdict() {
        Verdict::Pass => AuthoredCheckOutcome::Pass,
        Verdict::Regression => AuthoredCheckOutcome::Regression,
        Verdict::Unknown => AuthoredCheckOutcome::Unknown,
    };
    Ok(Some(AuthoredCheckResult {
        observed_results: observed_results(&completed.result, &definition),
        outcome,
    }))
}

fn observed_results(
    result: &AuthoredExperimentResult,
    definition: &AuthoredExperimentDefinition,
) -> Vec<AuthoredObservedResult> {
    result
        .criteria()
        .iter()
        .zip(&definition.criteria)
        .map(|(result, criterion)| AuthoredObservedResult {
            criterion_id: result.criterion_id().to_owned(),
            expected_maximum: result.expected_maximum(),
            expected_minimum: result.expected_minimum(),
            observed_maximum: result.observed_maximum(),
            observed_median: result.observed_median(),
            observed_minimum: result.observed_minimum(),
            unit: criterion.unit.clone(),
        })
        .collect()
}

pub fn authored_repro_ids(root: &Path) -> Result<Vec<ReproId>, Error> {
    let authored_root = root.join(".reproit/authored-repros");
    let metadata = match fs::symlink_metadata(&authored_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(storage_error()),
    };
    if !metadata.file_type().is_dir() {
        return Err(storage_error());
    }
    let mut repro_ids = fs::read_dir(authored_root)
        .map_err(|_| storage_error())?
        .take(MAX_AUTHORED_REPROS + 1)
        .map(|entry| {
            let entry = entry.map_err(|_| storage_error())?;
            if !entry.file_type().map_err(|_| storage_error())?.is_dir() {
                return Err(storage_error());
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| storage_error())?;
            ReproId::from_str(&name).map_err(|_| storage_error())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if repro_ids.len() > MAX_AUTHORED_REPROS {
        return Err(Error::new(
            ErrorCode::RuntimeQuota,
            "The authored Repro set exceeds its configured limit.",
        ));
    }
    repro_ids.sort_unstable();
    Ok(repro_ids)
}

fn open_authored_repro(
    root: &Path,
    repro_id: ReproId,
) -> Result<Option<OpenedAuthoredRepro>, Error> {
    let repro_root = root
        .join(".reproit/authored-repros")
        .join(repro_id.to_string());
    match fs::symlink_metadata(&repro_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(_) | Err(_) => return Err(storage_error()),
    }
    let Some(reference): Option<AuthoredReproReference> =
        read_canonical_optional(&repro_root.join("reference.json"))?
    else {
        return Ok(Some(OpenedAuthoredRepro::MissingEvidence));
    };
    if reference.repro_id != repro_id || reference.profile != PROFILE {
        return Err(Error::object_digest_mismatch());
    }
    let Some(definition): Option<AuthoredExperimentDefinition> =
        read_canonical_optional(&repro_root.join("definition.json"))?
    else {
        return Ok(Some(OpenedAuthoredRepro::MissingEvidence));
    };
    ProfileCapabilityRegistry::installed()
        .require(&definition.profile, AUTHORED_REPRO_CAPABILITY)?;
    if canonical::digest(&definition)? != reference.definition_digest {
        return Err(Error::object_digest_mismatch());
    }
    let Some(capsule): Option<AuthoredCapsule> =
        read_canonical_optional(&repro_root.join("capsule.json"))?
    else {
        return Ok(Some(OpenedAuthoredRepro::MissingEvidence));
    };
    if capsule.digest()? != reference.capsule_digest
        || derived_repro_id(capsule.digest()?)? != repro_id
        || capsule.repository_id() != reference.repository_id
    {
        return Err(Error::object_digest_mismatch());
    }
    let Some(initial_evidence): Option<AuthoredExecutionEvidence> =
        read_canonical_optional(&repro_root.join("initial-evidence.json"))?
    else {
        return Ok(Some(OpenedAuthoredRepro::MissingEvidence));
    };
    let Some(initial_result): Option<AuthoredExperimentResult> =
        read_canonical_optional(&repro_root.join("initial-result.json"))?
    else {
        return Ok(Some(OpenedAuthoredRepro::MissingEvidence));
    };
    let Some(claim): Option<AuthoredClaim> =
        read_canonical_optional(&repro_root.join("claim.json"))?
    else {
        return Ok(Some(OpenedAuthoredRepro::MissingEvidence));
    };
    if canonical::digest(&initial_evidence)? != reference.evidence_digest
        || canonical::digest(&initial_result)? != reference.result_digest
        || claim.digest()? != reference.claim_digest
    {
        return Err(Error::object_digest_mismatch());
    }
    let object_root = repro_root.join("objects");
    if !evidence_objects_present(&initial_evidence, &object_root)? {
        return Ok(Some(OpenedAuthoredRepro::MissingEvidence));
    }
    verify(
        &definition,
        &capsule,
        &initial_evidence,
        &initial_result,
        &object_root,
    )?;
    if initial_result.verdict() != Verdict::Pass {
        return Err(Error::object_digest_mismatch());
    }
    verify_authored_claim(
        &claim,
        &definition,
        capsule.digest()?,
        canonical::digest(&initial_result)?,
    )?;
    Ok(Some(OpenedAuthoredRepro::Ready {
        capsule,
        definition: Box::new(definition),
    }))
}

fn unknown_check_result() -> AuthoredCheckResult {
    AuthoredCheckResult {
        observed_results: Vec::new(),
        outcome: AuthoredCheckOutcome::Unknown,
    }
}

fn verify_completed(
    definition: &AuthoredExperimentDefinition,
    completed: &CompletedAuthoredExperiment,
) -> Result<(), Error> {
    verify(
        definition,
        &completed.capsule,
        &completed.evidence,
        &completed.result,
        &completed.object_root,
    )
}

fn derived_repro_id(capsule_digest: Digest) -> Result<ReproId, Error> {
    let mut input = b"reproit-authored-repro-id-v1".to_vec();
    input.extend_from_slice(capsule_digest.as_bytes());
    let mut bytes = Digest::of(&input).as_bytes()[..16].to_vec();
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "rpr_{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
    .parse()
}

fn require_initialized(root: &Path) -> Result<PathBuf, Error> {
    let config_root = root.join(".reproit");
    let config = config_root.join("project.toml");
    let root_metadata = fs::symlink_metadata(&config_root).map_err(|_| not_initialized())?;
    let config_metadata = fs::symlink_metadata(config).map_err(|_| not_initialized())?;
    if !root_metadata.file_type().is_dir() || !config_metadata.file_type().is_file() {
        return Err(not_initialized());
    }
    Ok(config_root)
}

fn resolve_regular_file(root: &Path, relative: &Path) -> Result<PathBuf, Error> {
    validate_relative_path(relative)?;
    let mut current = root.to_owned();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            return Err(Error::schema_invalid());
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|_| source_error())?;
        let last = index + 1 == component_count;
        if (last && !metadata.file_type().is_file()) || (!last && !metadata.file_type().is_dir()) {
            return Err(source_error());
        }
    }
    Ok(current)
}

fn validate_relative_path(path: &Path) -> Result<(), Error> {
    if path.as_os_str().is_empty()
        || path.as_os_str().len() > 512
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::schema_invalid());
    }
    Ok(())
}

fn read_bounded(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, Error> {
    let metadata = fs::symlink_metadata(path).map_err(|_| source_error())?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > maximum_bytes {
        return Err(source_error());
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| source_error())?;
    let mut bytes = Vec::with_capacity(capacity);
    File::open(path)
        .and_then(|file| file.take(maximum_bytes + 1).read_to_end(&mut bytes))
        .map_err(|_| source_error())?;
    if bytes.len() != capacity {
        return Err(source_error());
    }
    Ok(bytes)
}

fn read_canonical_optional<T: DeserializeOwned + Serialize>(
    path: &Path,
) -> Result<Option<T>, Error> {
    let Some(bytes) = read_stored_bounded_optional(path, MAX_STORED_JSON_BYTES)? else {
        return Ok(None);
    };
    let value: T = canonical::parse_strict(&bytes)?;
    if canonical::canonical_bytes(&value)? != bytes {
        return Err(Error::schema_invalid());
    }
    Ok(Some(value))
}

fn read_stored_bounded_optional(path: &Path, maximum_bytes: u64) -> Result<Option<Vec<u8>>, Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(storage_error()),
    };
    if !metadata.file_type().is_file() || metadata.len() > maximum_bytes {
        return Err(storage_error());
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| storage_error())?;
    let mut bytes = Vec::with_capacity(capacity);
    File::open(path)
        .and_then(|file| file.take(maximum_bytes + 1).read_to_end(&mut bytes))
        .map_err(|_| storage_error())?;
    if bytes.len() != capacity {
        return Err(storage_error());
    }
    Ok(Some(bytes))
}

fn write_canonical_new(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    let bytes = canonical::canonical_bytes(value)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| storage_error())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| storage_error())
}

fn copy_objects(source_root: &Path, destination_root: &Path) -> Result<(), Error> {
    let source = source_root.join("sha256");
    let destination = destination_root.join("sha256");
    fs::create_dir_all(&destination).map_err(|_| storage_error())?;
    let mut stored_count = validate_object_directory(&destination)?;
    let mut source_count = 0_usize;
    for entry in fs::read_dir(source).map_err(|_| storage_error())? {
        source_count = source_count.checked_add(1).ok_or_else(storage_error)?;
        if source_count > MAX_OBJECTS {
            return Err(storage_error());
        }
        let entry = entry.map_err(|_| storage_error())?;
        if !entry.file_type().map_err(|_| storage_error())?.is_file() {
            return Err(storage_error());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| storage_error())?;
        if name.len() != 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(storage_error());
        }
        let target = destination.join(name);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_file() => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) | Err(_) => return Err(storage_error()),
        }
        stored_count = stored_count.checked_add(1).ok_or_else(storage_error)?;
        if stored_count > MAX_OBJECTS {
            return Err(storage_error());
        }
        fs::copy(entry.path(), &target).map_err(|_| storage_error())?;
        OpenOptions::new()
            .write(true)
            .open(target)
            .and_then(|file| file.sync_all())
            .map_err(|_| storage_error())?;
    }
    Ok(())
}

fn validate_object_directory(directory: &Path) -> Result<usize, Error> {
    let mut count = 0_usize;
    for entry in fs::read_dir(directory).map_err(|_| storage_error())? {
        count = count.checked_add(1).ok_or_else(storage_error)?;
        if count > MAX_OBJECTS {
            return Err(storage_error());
        }
        let entry = entry.map_err(|_| storage_error())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| storage_error())?;
        if !entry.file_type().map_err(|_| storage_error())?.is_file()
            || name.len() != 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(storage_error());
        }
    }
    Ok(count)
}

fn publish_sealed(config_root: &Path, repro_id: ReproId, sealed: &Path) -> Result<(), Error> {
    let authored_root = config_root.join("authored-repros");
    match fs::create_dir(&authored_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&authored_root)
                .map_err(|_| storage_error())?
                .file_type()
                .is_dir()
            {
                return Err(storage_error());
            }
        }
        Err(_) => return Err(storage_error()),
    }
    let destination = authored_root.join(repro_id.to_string());
    if destination.exists() {
        return Err(Error::new(
            ErrorCode::ConfigConflict,
            "The authored Repro already exists.",
        ));
    }
    fs::rename(sealed, destination).map_err(|_| storage_error())
}

fn reference_path(repro_id: ReproId) -> String {
    format!(".reproit/authored-repros/{repro_id}/reference.json")
}

fn not_initialized() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        "Initialize the repository before you add an authored Repro.",
    )
}

fn source_error() -> Error {
    Error::new(
        ErrorCode::SourceAccessDenied,
        "The project repository or research recipe is not admissible.",
    )
}

fn range_failed() -> Error {
    Error::new(
        ErrorCode::DifferentFailure,
        "The observed results are outside the expected ranges.",
    )
}

fn storage_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not store the authored Repro.",
    )
}

fn archive_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not archive the authored experiment evidence.",
    )
}

fn publish_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not publish the authored Repro.",
    )
}
