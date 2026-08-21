use std::path::PathBuf;

use reproit_app::{
    KeptReferenceStore as _,
    agent::{
        AgentCheckResult, AgentFuture, AgentOperations, CheckReprosInput, CheckReprosResult,
        CheckStatus, GetReproInput, KeepReproResult, KeptReproSummary, ListReprosInput,
        ListReprosResult, ReproScope, RunReproInput, RunReproResult, RunStatus, RunSubject,
        TriageReproInput, TriageReproResult,
    },
    list_kept,
};
use reproit_backend::config::ProjectConfig;
use reproit_cloud_api::{
    ManagedKeepRequest, ManagedOciGrant, ManagedOciGrantRequest, ManagedOperation,
    OccurrenceListQuery, OccurrenceSummary, Priority, ReproDetail, ReproListQuery, Triage,
    Workflow,
};
use reproit_core::{
    Error, ErrorCode,
    identity::{Digest, ReproId},
    model::{ExecutionOutcome, ProcessingMode, replay_capabilities_present},
};
use secrecy::ExposeSecret as _;

use crate::{
    FilesystemRepository, GitSourceWorkspace, NativeCredentialStore, SourceCheckout,
    cloud::HttpCloudClient,
    current_project_source,
    executor_control::{
        ExecutorControl, ManagedExecutionSession, WorkerSubject, random_debugger_capability,
    },
};

const CLOUD_ORIGIN: &str = "https://cloud.reproit.com";
const MAX_CLOUD_SCAN: usize = 10_000;
const MAX_OCCURRENCE_SCAN: usize = 10_000;

pub struct ProductionAgent {
    root: PathBuf,
}

struct ManagedRunRequest {
    attach_debugger: bool,
    occurrence: OccurrenceSummary,
    operation: ManagedOperation,
    subject: WorkerSubject,
}

impl ProductionAgent {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn repository(&self) -> FilesystemRepository {
        FilesystemRepository::new(self.root.clone())
    }

    pub async fn debug(
        &self,
        repro_id: ReproId,
    ) -> Result<reproit_core::model::ExecutionResult, Error> {
        let project = self.repository().read_project()?;
        require_managed_project(&project)?;
        let cloud = cloud_client()?;
        let detail = cloud.get_repro(repro_id).await?;
        require_managed_repro(&detail)?;
        self.run_managed(
            &cloud,
            &project,
            &detail,
            ManagedOperation::Debug,
            WorkerSubject::Captured,
            true,
        )
        .await
    }

    async fn run_managed(
        &self,
        cloud: &HttpCloudClient,
        project: &ProjectConfig,
        detail: &ReproDetail,
        operation: ManagedOperation,
        subject: WorkerSubject,
        attach_debugger: bool,
    ) -> Result<reproit_core::model::ExecutionResult, Error> {
        self.run_managed_completed(cloud, project, detail, operation, subject, attach_debugger)
            .await
            .map(|(result, _)| result)
    }

    async fn run_managed_completed(
        &self,
        cloud: &HttpCloudClient,
        project: &ProjectConfig,
        detail: &ReproDetail,
        operation: ManagedOperation,
        subject: WorkerSubject,
        attach_debugger: bool,
    ) -> Result<(reproit_core::model::ExecutionResult, ManagedOciGrant), Error> {
        let occurrence = select_debug_occurrence(cloud, detail).await?;
        self.run_managed_occurrence(
            cloud,
            project,
            detail,
            ManagedRunRequest {
                attach_debugger,
                occurrence,
                operation,
                subject,
            },
        )
        .await
    }

    async fn run_managed_occurrence(
        &self,
        cloud: &HttpCloudClient,
        project: &ProjectConfig,
        detail: &ReproDetail,
        request: ManagedRunRequest,
    ) -> Result<(reproit_core::model::ExecutionResult, ManagedOciGrant), Error> {
        let debugger_capability = random_debugger_capability()?;
        let grant = cloud
            .create_managed_grant(
                detail.summary.repro_id,
                &ManagedOciGrantRequest {
                    capture_batch_digest: request.occurrence.capture_batch_digest,
                    debugger_capability_digest: Digest::of(
                        debugger_capability.expose_secret().as_bytes(),
                    ),
                    operation: request.operation,
                },
            )
            .await?;
        if grant.capture_id != request.occurrence.capture_id
            || grant.capsule_digest != request.occurrence.capsule_digest
        {
            cloud.cancel_managed_grant(&grant).await?;
            return Err(Error::object_digest_mismatch());
        }
        let control = match ExecutorControl::load_for_project(project) {
            Ok(control) => control,
            Err(error) => {
                cloud.cancel_managed_grant(&grant).await?;
                return Err(error);
            }
        };
        let session = match control
            .open_managed(
                &request.occurrence.required_capabilities,
                project,
                grant.clone(),
                debugger_capability,
            )
            .await
        {
            Ok(session) => session,
            Err(error) => {
                cloud.cancel_managed_grant(&grant).await?;
                return Err(error);
            }
        };
        let execution = self
            .execute_managed(project, detail, &session, &request)
            .await;
        let grant = session.grant().clone();
        let cleanup = cloud.cancel_managed_grant(&grant).await;
        match (execution, cleanup) {
            (_, Err(_)) => Err(cleanup_failed()),
            (Err(error), Ok(())) => Err(error),
            (Ok(result), Ok(())) => Ok((result, grant)),
        }
    }

    async fn execute_managed(
        &self,
        project: &ProjectConfig,
        detail: &ReproDetail,
        session: &ManagedExecutionSession,
        request: &ManagedRunRequest,
    ) -> Result<reproit_core::model::ExecutionResult, Error> {
        let configuration = reproit_core::canonical::canonical_bytes(project)?;
        if request.subject == WorkerSubject::Changed {
            let source = current_project_source(&self.root, project)?;
            return session
                .execute(
                    request.attach_debugger,
                    &configuration,
                    detail.summary.repro_id,
                    &source,
                    request.subject,
                )
                .await;
        }
        let occurrence = &request.occurrence;
        if occurrence.capture_id != session.grant().capture_id
            || occurrence.capture_batch_digest != session.grant().capture_batch_digest
            || occurrence.capsule_digest != session.grant().capsule_digest
        {
            return Err(Error::object_digest_mismatch());
        }
        let workspace = GitSourceWorkspace::new(&self.root, project)?;
        let source = workspace.prepare(&SourceCheckout {
            repository_id: project.repository_id.clone(),
            source_revision: occurrence.source_revision.clone(),
        })?;
        let result = session
            .execute(
                request.attach_debugger,
                &configuration,
                detail.summary.repro_id,
                &source,
                request.subject,
            )
            .await;
        let cleanup = workspace.cleanup(&source);
        match (result, cleanup) {
            (_, Err(_)) => Err(cleanup_failed()),
            (Err(error), Ok(())) => Err(error),
            (Ok(result), Ok(())) => Ok(result),
        }
    }

    async fn list_cloud(
        &self,
        input: &ListReprosInput,
        project: &ProjectConfig,
    ) -> Result<ListReprosResult, Error> {
        let cloud = cloud_client()?;
        let priorities = optional_values(&input.priority);
        let workflows = optional_values(&input.workflow);
        let mut repros = Vec::new();
        for priority in priorities {
            for workflow in &workflows {
                let mut cursor = None;
                loop {
                    let page = cloud
                        .list_repros(&ReproListQuery {
                            assignee_id: input.assignee_id.clone(),
                            cursor,
                            limit: Some(100),
                            priority,
                            project_id: Some(project.project_id),
                            service_id: None,
                            workflow: *workflow,
                        })
                        .await?;
                    if repros.len().saturating_add(page.repros.len()) > MAX_CLOUD_SCAN {
                        return Err(quota_error(
                            "The Cloud Repro scan exceeds its configured limit.",
                        ));
                    }
                    repros.extend(page.repros);
                    let Some(next) = page.next_cursor else { break };
                    cursor = Some(next);
                }
            }
        }
        sort_repros(&mut repros);
        repros.dedup_by_key(|repro| repro.repro_id);
        let offset = parse_cursor(input.cursor.as_deref(), repros.len())?;
        let end = offset
            .saturating_add(usize::from(input.limit))
            .min(repros.len());
        Ok(ListReprosResult::Cloud {
            next_cursor: (end < repros.len()).then(|| end.to_string()),
            repros: repros[offset..end].to_vec(),
        })
    }

    fn list_local(&self, input: &ListReprosInput) -> Result<ListReprosResult, Error> {
        if input.assignee_id.is_some() || !input.priority.is_empty() || !input.workflow.is_empty() {
            return Ok(ListReprosResult::Kept {
                next_cursor: None,
                repros: Vec::new(),
            });
        }
        let references = list_kept(&self.repository())?;
        let offset = parse_cursor(input.cursor.as_deref(), references.len())?;
        let end = offset
            .saturating_add(usize::from(input.limit))
            .min(references.len());
        let repros = references[offset..end]
            .iter()
            .map(|reference| KeptReproSummary {
                capsule_digest: reference.capsule_digest,
                capture_batch_digest: reference.capture_batch_digest,
                capture_id: reference.capture_id,
                processing_mode: reference.processing_mode,
                profile: reference.profile.clone(),
                repro_id: reference.repro_id,
                tracked_reference_path: tracked_reference_path(reference.repro_id),
            })
            .collect();
        Ok(ListReprosResult::Kept {
            next_cursor: (end < references.len()).then(|| end.to_string()),
            repros,
        })
    }

    async fn run_inner(&self, input: RunReproInput) -> Result<RunReproResult, Error> {
        let project = self.repository().read_project()?;
        require_managed_project(&project)?;
        let cloud = cloud_client()?;
        let detail = cloud.get_repro(input.repro_id).await?;
        require_managed_repro(&detail)?;
        let (operation, subject) = match input.subject {
            RunSubject::Captured => (ManagedOperation::Replay, WorkerSubject::Captured),
            RunSubject::Developer => (ManagedOperation::Check, WorkerSubject::Changed),
        };
        let execution = self
            .run_managed(&cloud, &project, &detail, operation, subject, false)
            .await?;
        run_result(input.repro_id, execution)
    }

    async fn check_one(&self, repro_id: ReproId) -> Result<CheckStatus, Error> {
        let result = self
            .run_inner(RunReproInput {
                repro_id,
                subject: RunSubject::Developer,
            })
            .await?;
        match result.status {
            RunStatus::Pass => Ok(CheckStatus::Pass),
            RunStatus::Regression => Ok(CheckStatus::Regression),
            RunStatus::Error => Err(Error::schema_invalid()),
        }
    }

    pub async fn check_repros_streaming(
        &self,
        input: CheckReprosInput,
        mut sink: impl FnMut(&AgentCheckResult) -> Result<(), Error>,
    ) -> Result<CheckReprosResult, Error> {
        input.validate()?;
        let repro_ids = if input.repro_ids.is_empty() {
            let project = self.repository().read_project()?;
            require_managed_project(&project)?;
            list_kept(&self.repository())?
                .into_iter()
                .map(|reference| reference.repro_id)
                .collect()
        } else {
            input.repro_ids
        };
        let mut summary = CheckReprosResult::empty();
        for repro_id in repro_ids {
            let result = check_result(repro_id, self.check_one(repro_id).await);
            sink(&result)?;
            summary.record(result)?;
        }
        summary.validate()?;
        Ok(summary)
    }

    async fn keep_inner(&self, repro_id: ReproId) -> Result<KeepReproResult, Error> {
        let mut store = self.repository();
        let project = store.read_project()?;
        require_managed_project(&project)?;
        let cloud = cloud_client()?;
        let detail = cloud.get_repro(repro_id).await?;
        require_managed_repro(&detail)?;
        let (execution, grant) = self
            .run_managed_completed(
                &cloud,
                &project,
                &detail,
                ManagedOperation::Keep,
                WorkerSubject::Changed,
                false,
            )
            .await?;
        if changed_status(&execution)? != CheckStatus::Pass {
            return Err(Error::new(
                ErrorCode::DifferentFailure,
                "The stored Failure still reproduces.",
            ));
        }
        let result = cloud
            .keep_managed(repro_id, &ManagedKeepRequest { grant })
            .await?;
        store.write_kept(&result.reference)?;
        Ok(KeepReproResult {
            capture_batch_digest: result.reference.capture_batch_digest,
            processing_mode: result.reference.processing_mode,
            registry_manifest_digest: reference_digest(&result.reference.capture_batch)?,
            repro_id,
            tracked_reference_path: tracked_reference_path(repro_id),
        })
    }
}

impl AgentOperations for ProductionAgent {
    fn list_repros(&self, input: ListReprosInput) -> AgentFuture<'_, ListReprosResult> {
        Box::pin(async move {
            input.validate()?;
            let project = self.repository().read_project()?;
            require_managed_project(&project)?;
            let result = match input.scope {
                ReproScope::Cloud => self.list_cloud(&input, &project).await?,
                ReproScope::Kept => self.list_local(&input)?,
            };
            result.validate()?;
            Ok(result)
        })
    }

    fn get_repro(&self, input: GetReproInput) -> AgentFuture<'_, ReproDetail> {
        Box::pin(async move {
            require_managed_project(&self.repository().read_project()?)?;
            let detail = cloud_client()?.get_repro(input.repro_id).await?;
            require_managed_repro(&detail)?;
            Ok(detail)
        })
    }

    fn triage_repro(&self, input: TriageReproInput) -> AgentFuture<'_, TriageReproResult> {
        Box::pin(async move {
            input.validate()?;
            require_managed_project(&self.repository().read_project()?)?;
            let cloud = cloud_client()?;
            let detail = cloud.get_repro(input.repro_id).await?;
            require_managed_repro(&detail)?;
            let previous = Triage {
                assignee_id: detail.summary.assignee_id,
                priority: detail.summary.priority,
                triage_revision: detail.summary.triage_revision,
                workflow: detail.summary.workflow,
            };
            if previous.triage_revision != input.triage_revision {
                return Err(Error::new(
                    ErrorCode::TriageConflict,
                    "The triage revision does not match the current Repro.",
                ));
            }
            let next = Triage {
                assignee_id: input.assignee_id,
                priority: input.priority,
                triage_revision: input.triage_revision,
                workflow: input.workflow,
            };
            if next.assignee_id == previous.assignee_id
                && next.priority == previous.priority
                && next.workflow == previous.workflow
            {
                return Err(Error::new(
                    ErrorCode::ConfigConflict,
                    "The triage operation does not change the Repro.",
                ));
            }
            if previous.workflow != Workflow::Resolved && next.workflow == Workflow::Resolved {
                let check = self
                    .run_inner(RunReproInput {
                        repro_id: input.repro_id,
                        subject: RunSubject::Developer,
                    })
                    .await?;
                if check.status != RunStatus::Pass {
                    return Err(Error::new(
                        ErrorCode::DifferentFailure,
                        "The changed source check did not pass.",
                    ));
                }
            }
            let current = cloud.update_triage(input.repro_id, &next).await?;
            let result = TriageReproResult {
                current,
                previous,
                repro_id: input.repro_id,
            };
            result.validate()?;
            Ok(result)
        })
    }

    fn run_repro(&self, input: RunReproInput) -> AgentFuture<'_, RunReproResult> {
        Box::pin(async move {
            let repro_id = input.repro_id;
            let result = match self.run_inner(input).await {
                Ok(result) => result,
                Err(error) => RunReproResult {
                    error: Some(error),
                    evidence_paths: Vec::new(),
                    execution: None,
                    repro_id,
                    status: RunStatus::Error,
                    workspace: None,
                },
            };
            result.validate()?;
            Ok(result)
        })
    }

    fn check_repros(&self, input: CheckReprosInput) -> AgentFuture<'_, CheckReprosResult> {
        Box::pin(async move { self.check_repros_streaming(input, |_| Ok(())).await })
    }

    fn keep_repro(&self, input: GetReproInput) -> AgentFuture<'_, KeepReproResult> {
        Box::pin(async move {
            let result = self.keep_inner(input.repro_id).await?;
            result.validate()?;
            Ok(result)
        })
    }
}

async fn select_debug_occurrence(
    cloud: &HttpCloudClient,
    detail: &ReproDetail,
) -> Result<OccurrenceSummary, Error> {
    scan_occurrences(cloud, detail.summary.repro_id, |occurrence| {
        replay_capabilities_present(
            &occurrence.required_capabilities,
            &detail.required_capabilities,
        )
    })
    .await?
    .ok_or_else(|| {
        Error::new(
            ErrorCode::UnsupportedCapabilitySet,
            "No retained occurrence is compatible with the replay host.",
        )
    })
}

async fn scan_occurrences(
    cloud: &HttpCloudClient,
    repro_id: ReproId,
    mut matches: impl FnMut(&OccurrenceSummary) -> bool,
) -> Result<Option<OccurrenceSummary>, Error> {
    let mut cursor = None;
    let mut scanned = 0_usize;
    loop {
        let page = cloud
            .list_occurrences(
                repro_id,
                &OccurrenceListQuery {
                    cursor,
                    limit: Some(100),
                },
            )
            .await?;
        for occurrence in page.occurrences {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_OCCURRENCE_SCAN {
                return Err(quota_error(
                    "The retained occurrence scan exceeds its configured limit.",
                ));
            }
            if matches(&occurrence) {
                return Ok(Some(occurrence));
            }
        }
        let Some(next) = page.next_cursor else {
            return Ok(None);
        };
        cursor = Some(next);
    }
}

fn require_managed_project(project: &ProjectConfig) -> Result<(), Error> {
    if project.processing_mode == ProcessingMode::Managed {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::ConfigConflict,
            "This Repro It release supports managed projects only.",
        ))
    }
}

fn require_managed_repro(detail: &ReproDetail) -> Result<(), Error> {
    if detail.processing_mode == ProcessingMode::Managed {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::ConfigConflict,
            "This Repro is not available through the managed product.",
        ))
    }
}

fn run_result(
    repro_id: ReproId,
    execution: reproit_core::model::ExecutionResult,
) -> Result<RunReproResult, Error> {
    let status = match execution.result {
        ExecutionOutcome::TargetAbsent => RunStatus::Pass,
        ExecutionOutcome::TargetReproduced => RunStatus::Regression,
        _ => return Err(evaluation_error()),
    };
    Ok(RunReproResult {
        error: None,
        evidence_paths: Vec::new(),
        execution: Some(execution),
        repro_id,
        status,
        workspace: None,
    })
}

fn changed_status(execution: &reproit_core::model::ExecutionResult) -> Result<CheckStatus, Error> {
    match execution.result {
        ExecutionOutcome::TargetAbsent => Ok(CheckStatus::Pass),
        ExecutionOutcome::TargetReproduced => Ok(CheckStatus::Regression),
        ExecutionOutcome::DifferentFailure => Err(Error::new(
            ErrorCode::DifferentFailure,
            "The changed source produced a different Failure.",
        )),
        _ => Err(evaluation_error()),
    }
}

fn check_result(repro_id: ReproId, result: Result<CheckStatus, Error>) -> AgentCheckResult {
    match result {
        Ok(status) => AgentCheckResult {
            error: None,
            repro_id,
            status,
        },
        Err(error) => AgentCheckResult {
            error: Some(error),
            repro_id,
            status: CheckStatus::Error,
        },
    }
}

fn sort_repros(repros: &mut [reproit_cloud_api::ReproSummary]) {
    repros.sort_by(|left, right| {
        priority_order(left.priority)
            .cmp(&priority_order(right.priority))
            .then_with(|| left.assignee_id.is_some().cmp(&right.assignee_id.is_some()))
            .then_with(|| right.latest_seen_at.cmp(&left.latest_seen_at))
            .then_with(|| left.repro_id.cmp(&right.repro_id))
    });
}

const fn priority_order(priority: Priority) -> u8 {
    match priority {
        Priority::P0 => 0,
        Priority::P1 => 1,
        Priority::Unset => 2,
        Priority::P2 => 3,
        Priority::P3 => 4,
    }
}

fn optional_values<T: Copy>(values: &[T]) -> Vec<Option<T>> {
    if values.is_empty() {
        vec![None]
    } else {
        values.iter().copied().map(Some).collect()
    }
}

fn parse_cursor(cursor: Option<&str>, length: usize) -> Result<usize, Error> {
    let offset = cursor.map_or(Ok(0), |value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|offset| offset.to_string() == value)
            .ok_or_else(Error::schema_invalid)
    })?;
    (offset <= length)
        .then_some(offset)
        .ok_or_else(Error::schema_invalid)
}

fn tracked_reference_path(repro_id: ReproId) -> String {
    format!(".reproit/repros/{repro_id}.toml")
}

fn reference_digest(reference: &str) -> Result<Digest, Error> {
    reference
        .rsplit_once('@')
        .ok_or_else(Error::schema_invalid)?
        .1
        .parse()
}

fn cloud_client() -> Result<HttpCloudClient, Error> {
    let session = NativeCredentialStore::open()?.load()?;
    let origin = std::env::var("REPROIT_CLOUD_ORIGIN").unwrap_or_else(|_| CLOUD_ORIGIN.to_owned());
    HttpCloudClient::new(&origin, session)
}

fn cleanup_failed() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The replay host cleanup did not complete.",
    )
}

fn evaluation_error() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "The isolated execution did not return a valid result.",
    )
}

fn quota_error(message: &'static str) -> Error {
    Error::new(ErrorCode::RuntimeQuota, message)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn cursors_are_canonical_and_bounded() {
        assert_eq!(parse_cursor(None, 2).unwrap(), 0);
        assert_eq!(parse_cursor(Some("2"), 2).unwrap(), 2);
        assert!(parse_cursor(Some("02"), 2).is_err());
        assert!(parse_cursor(Some("3"), 2).is_err());
    }

    #[test]
    fn optional_filters_preserve_explicit_values() {
        assert_eq!(optional_values::<Priority>(&[]), vec![None]);
        assert_eq!(
            optional_values(&[Priority::P0, Priority::P2]),
            vec![Some(Priority::P0), Some(Priority::P2)]
        );
    }

    #[test]
    fn duplicate_ids_can_be_removed_after_sorting() {
        let mut identities = BTreeSet::new();
        assert!(identities.insert("rpr_01890f3e-7b1c-7cc0-8a1b-123456789abc"));
        assert!(!identities.insert("rpr_01890f3e-7b1c-7cc0-8a1b-123456789abc"));
    }
}
