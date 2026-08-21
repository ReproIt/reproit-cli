use std::{future::Future, pin::Pin};

use reproit_cloud_api::{Priority, ReproDetail, ReproSummary, Triage, Workflow};
use reproit_core::{
    Error, ErrorCode,
    identity::{CaptureId, Digest, ReproId},
    model::{ExecutionOutcome, ExecutionResult, Validate},
};
use serde::{Deserialize, Serialize};

use crate::MAX_KEPT_REFERENCES;

pub const MAX_TOOL_RESULTS: usize = 100;
pub const MAX_EVIDENCE_PATHS: usize = 16;
pub const MAX_CURSOR_BYTES: usize = 2_048;
pub const MAX_PATH_BYTES: usize = 4_096;
pub const MAX_ASSIGNEE_BYTES: usize = 256;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub type AgentFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

pub trait AgentOperations: Send + Sync {
    fn check_repros(&self, input: CheckReprosInput) -> AgentFuture<'_, CheckReprosResult>;
    fn get_repro(&self, input: GetReproInput) -> AgentFuture<'_, ReproDetail>;
    fn keep_repro(&self, input: GetReproInput) -> AgentFuture<'_, KeepReproResult>;
    fn list_repros(&self, input: ListReprosInput) -> AgentFuture<'_, ListReprosResult>;
    fn run_repro(&self, input: RunReproInput) -> AgentFuture<'_, RunReproResult>;
    fn triage_repro(&self, input: TriageReproInput) -> AgentFuture<'_, TriageReproResult>;
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReproScope {
    Cloud,
    Kept,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListReprosInput {
    #[serde(deserialize_with = "required_nullable")]
    pub assignee_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub cursor: Option<String>,
    pub limit: u16,
    pub priority: Vec<Priority>,
    pub scope: ReproScope,
    pub workflow: Vec<Workflow>,
}

impl ListReprosInput {
    pub fn validate(&self) -> Result<(), Error> {
        if self.limit == 0
            || usize::from(self.limit) > MAX_TOOL_RESULTS
            || self.priority.len() > 5
            || self.workflow.len() > 3
            || !unique(&self.priority)
            || !unique(&self.workflow)
            || self
                .cursor
                .as_ref()
                .is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES)
            || self
                .assignee_id
                .as_ref()
                .is_some_and(|assignee| assignee.is_empty() || assignee.len() > MAX_ASSIGNEE_BYTES)
        {
            return Err(Error::schema_invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "lowercase")]
pub enum ListReprosResult {
    Cloud {
        next_cursor: Option<String>,
        repros: Vec<ReproSummary>,
    },
    Kept {
        next_cursor: Option<String>,
        repros: Vec<KeptReproSummary>,
    },
}

impl ListReprosResult {
    pub fn validate(&self) -> Result<(), Error> {
        let (length, cursor) = match self {
            Self::Cloud {
                next_cursor,
                repros,
            } => (repros.len(), next_cursor),
            Self::Kept {
                next_cursor,
                repros,
            } => {
                for repro in repros {
                    validate_repository_path(&repro.tracked_reference_path)?;
                }
                (repros.len(), next_cursor)
            }
        };
        if length > MAX_TOOL_RESULTS
            || cursor
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_CURSOR_BYTES)
        {
            return Err(Error::schema_invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeptReproSummary {
    pub capsule_digest: Digest,
    pub capture_batch_digest: Digest,
    pub capture_id: CaptureId,
    pub processing_mode: reproit_core::model::ProcessingMode,
    pub profile: String,
    pub repro_id: ReproId,
    pub tracked_reference_path: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetReproInput {
    pub repro_id: ReproId,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageReproInput {
    #[serde(deserialize_with = "required_nullable")]
    pub assignee_id: Option<String>,
    pub priority: Priority,
    pub repro_id: ReproId,
    pub triage_revision: u64,
    pub workflow: Workflow,
}

impl TriageReproInput {
    pub fn validate(&self) -> Result<(), Error> {
        if self.triage_revision == 0
            || self.triage_revision > MAX_SAFE_INTEGER
            || self.assignee_id.as_ref().is_some_and(|assignee| {
                assignee.is_empty()
                    || assignee == "UNASSIGNED"
                    || assignee.len() > MAX_ASSIGNEE_BYTES
            })
        {
            return Err(Error::schema_invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct TriageReproResult {
    pub current: Triage,
    pub previous: Triage,
    pub repro_id: ReproId,
}

impl TriageReproResult {
    pub fn validate(&self) -> Result<(), Error> {
        if self.current.triage_revision != self.previous.triage_revision.saturating_add(1) {
            return Err(Error::new(
                ErrorCode::TriageConflict,
                "The triage revision did not advance exactly once.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunSubject {
    Captured,
    Developer,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunReproInput {
    pub repro_id: ReproId,
    pub subject: RunSubject,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    Pass,
    Regression,
    Error,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunReproResult {
    pub error: Option<Error>,
    pub evidence_paths: Vec<String>,
    pub execution: Option<ExecutionResult>,
    pub repro_id: ReproId,
    pub status: RunStatus,
    pub workspace: Option<String>,
}

impl RunReproResult {
    pub fn validate(&self) -> Result<(), Error> {
        if self.evidence_paths.len() > MAX_EVIDENCE_PATHS
            || !unique(&self.evidence_paths)
            || self
                .evidence_paths
                .iter()
                .any(|path| validate_repository_path(path).is_err())
            || self
                .workspace
                .as_ref()
                .is_some_and(|path| validate_repository_path(path).is_err())
        {
            return Err(Error::schema_invalid());
        }
        match (&self.status, &self.execution, &self.error) {
            (RunStatus::Pass, Some(execution), None)
                if execution.result == ExecutionOutcome::TargetAbsent =>
            {
                execution.validate()
            }
            (RunStatus::Regression, Some(execution), None)
                if execution.result == ExecutionOutcome::TargetReproduced =>
            {
                execution.validate()
            }
            (RunStatus::Error, None, Some(_)) => Ok(()),
            _ => Err(Error::schema_invalid()),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReprosInput {
    pub repro_ids: Vec<ReproId>,
}

impl CheckReprosInput {
    pub fn validate(&self) -> Result<(), Error> {
        if self.repro_ids.len() > MAX_TOOL_RESULTS || !unique(&self.repro_ids) {
            return Err(Error::schema_invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckStatus {
    Pass,
    Regression,
    Error,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentCheckResult {
    pub error: Option<Error>,
    pub repro_id: ReproId,
    pub status: CheckStatus,
}

impl AgentCheckResult {
    pub fn validate(&self) -> Result<(), Error> {
        match (self.status, &self.error) {
            (CheckStatus::Pass | CheckStatus::Regression, None) | (CheckStatus::Error, Some(_)) => {
                Ok(())
            }
            _ => Err(Error::schema_invalid()),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckReprosResult {
    pub checked_count: usize,
    pub error_count: usize,
    pub errors: Vec<AgentCheckResult>,
    pub pass_count: usize,
    pub regression_count: usize,
    pub regressions: Vec<AgentCheckResult>,
}

impl CheckReprosResult {
    pub fn empty() -> Self {
        Self {
            checked_count: 0,
            error_count: 0,
            errors: Vec::new(),
            pass_count: 0,
            regression_count: 0,
            regressions: Vec::new(),
        }
    }

    pub fn record(&mut self, result: AgentCheckResult) -> Result<(), Error> {
        result.validate()?;
        if self.checked_count >= MAX_KEPT_REFERENCES {
            return Err(Error::new(
                ErrorCode::RuntimeQuota,
                "The kept Repro set exceeds its configured limit.",
            ));
        }
        self.checked_count += 1;
        match result.status {
            CheckStatus::Pass => self.pass_count += 1,
            CheckStatus::Regression => {
                self.regression_count += 1;
                if self.regressions.len() < MAX_TOOL_RESULTS {
                    self.regressions.push(result);
                }
            }
            CheckStatus::Error => {
                self.error_count += 1;
                if self.errors.len() < MAX_TOOL_RESULTS {
                    self.errors.push(result);
                }
            }
        }
        Ok(())
    }

    pub fn from_results(
        results: impl IntoIterator<Item = AgentCheckResult>,
    ) -> Result<Self, Error> {
        let mut summary = Self::empty();
        for result in results {
            summary.record(result)?;
        }
        summary.validate()?;
        Ok(summary)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.checked_count > MAX_KEPT_REFERENCES
            || self.pass_count > self.checked_count
            || self.regression_count > self.checked_count
            || self.error_count > self.checked_count
            || self
                .pass_count
                .checked_add(self.regression_count)
                .and_then(|count| count.checked_add(self.error_count))
                != Some(self.checked_count)
            || self.regressions.len() > MAX_TOOL_RESULTS
            || self.errors.len() > MAX_TOOL_RESULTS
            || self.regressions.len() > self.regression_count
            || self.errors.len() > self.error_count
        {
            return Err(Error::schema_invalid());
        }
        for result in &self.regressions {
            if result.status != CheckStatus::Regression {
                return Err(Error::schema_invalid());
            }
            result.validate()?;
        }
        for result in &self.errors {
            if result.status != CheckStatus::Error {
                return Err(Error::schema_invalid());
            }
            result.validate()?;
        }
        if !unique_check_ids(self.regressions.iter().chain(&self.errors)) {
            return Err(Error::schema_invalid());
        }
        Ok(())
    }
}

fn unique_check_ids<'a>(mut results: impl Iterator<Item = &'a AgentCheckResult>) -> bool {
    let mut identities = std::collections::BTreeSet::new();
    results.all(|result| identities.insert(result.repro_id))
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeepReproResult {
    pub capture_batch_digest: Digest,
    pub processing_mode: reproit_core::model::ProcessingMode,
    pub registry_manifest_digest: Digest,
    pub repro_id: ReproId,
    pub tracked_reference_path: String,
}

impl KeepReproResult {
    pub fn validate(&self) -> Result<(), Error> {
        validate_repository_path(&self.tracked_reference_path)
    }
}

fn unique<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .all(|(index, value)| !values[..index].contains(value))
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub fn validate_repository_path(path: &str) -> Result<(), Error> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.as_bytes().get(1) == Some(&b':')
        || path.split('/').any(|component| component == "..")
    {
        return Err(Error::schema_invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repro_id(index: u16) -> ReproId {
        format!("rpr_01890f3e-7b1c-7cc0-8a1b-{index:012x}")
            .parse()
            .expect("valid fixture Repro identity")
    }

    #[test]
    fn check_summary_retains_only_bounded_failures_and_errors() {
        let mut summary = CheckReprosResult::empty();
        for index in 0..250_u16 {
            let status = match index % 3 {
                0 => CheckStatus::Pass,
                1 => CheckStatus::Regression,
                _ => CheckStatus::Error,
            };
            summary
                .record(AgentCheckResult {
                    error: (status == CheckStatus::Error).then(Error::schema_invalid),
                    repro_id: repro_id(index),
                    status,
                })
                .expect("bounded result");
        }
        assert_eq!(summary.checked_count, 250);
        assert_eq!(summary.pass_count, 84);
        assert_eq!(summary.regression_count, 83);
        assert_eq!(summary.error_count, 83);
        assert_eq!(summary.regressions.len(), 83);
        assert_eq!(summary.errors.len(), 83);
        summary.validate().expect("valid summary");
    }

    #[test]
    fn check_summary_rejects_one_result_over_the_kept_set_bound() {
        let mut summary = CheckReprosResult {
            checked_count: MAX_KEPT_REFERENCES,
            error_count: 0,
            errors: Vec::new(),
            pass_count: MAX_KEPT_REFERENCES,
            regression_count: 0,
            regressions: Vec::new(),
        };
        let error = summary
            .record(AgentCheckResult {
                error: None,
                repro_id: repro_id(1),
                status: CheckStatus::Pass,
            })
            .expect_err("one result over must fail");
        assert_eq!(error.code, ErrorCode::RuntimeQuota);
    }

    #[test]
    fn check_summary_rejects_count_and_retained_status_mismatch() {
        let summary = CheckReprosResult {
            checked_count: 1,
            error_count: 0,
            errors: Vec::new(),
            pass_count: 1,
            regression_count: 0,
            regressions: vec![AgentCheckResult {
                error: None,
                repro_id: repro_id(1),
                status: CheckStatus::Pass,
            }],
        };
        assert_eq!(
            summary.validate().expect_err("mismatch must fail").code,
            ErrorCode::SchemaInvalid
        );
    }
}
