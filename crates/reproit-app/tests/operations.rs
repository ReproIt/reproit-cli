use std::collections::BTreeMap;

use reproit_app::{KeptReferenceStore, ProjectStore, initialize, list_kept, remove_kept};
use reproit_core::{
    Error, ErrorCode, canonical,
    identity::ReproId,
    model::{KeptReference, Validate},
};
use serde_json::Value;

const VECTORS: &str = include_str!("../../../specs/v1/protocol-vectors.json");

#[derive(Default)]
struct MemoryStore {
    kept: BTreeMap<ReproId, KeptReference>,
    project: Option<TestProject>,
}

#[derive(Clone, Eq, PartialEq)]
struct TestProject {
    revision: u8,
}

impl Validate for TestProject {
    fn validate(&self) -> Result<(), Error> {
        (self.revision > 0)
            .then_some(())
            .ok_or_else(Error::schema_invalid)
    }
}

impl KeptReferenceStore for MemoryStore {
    fn list_kept(&self, limit: usize) -> Result<Vec<KeptReference>, Error> {
        Ok(self.kept.values().take(limit).cloned().collect())
    }

    fn remove_kept(&mut self, repro_id: ReproId) -> Result<bool, Error> {
        Ok(self.kept.remove(&repro_id).is_some())
    }

    fn write_kept(&mut self, reference: &KeptReference) -> Result<(), Error> {
        self.kept.insert(reference.repro_id, reference.clone());
        Ok(())
    }
}

impl ProjectStore<TestProject> for MemoryStore {
    fn is_initialized(&self) -> Result<bool, Error> {
        Ok(self.project.is_some())
    }

    fn read_project(&self) -> Result<Option<TestProject>, Error> {
        Ok(self.project.clone())
    }

    fn write_project(&mut self, config: &TestProject) -> Result<(), Error> {
        self.project = Some(config.clone());
        Ok(())
    }
}

#[test]
fn initialization_is_validated_idempotent_and_editable() {
    let (config, _) = fixtures();
    let mut store = MemoryStore::default();
    assert_eq!(
        initialize(&mut store, &config).unwrap(),
        reproit_app::InitializationResult::Created
    );
    assert_eq!(
        initialize(&mut store, &config).unwrap(),
        reproit_app::InitializationResult::Unchanged
    );

    let mut updated = config.clone();
    updated.revision = 2;
    assert_eq!(
        initialize(&mut store, &updated).unwrap(),
        reproit_app::InitializationResult::Updated
    );

    let mut invalid = config;
    invalid.revision = 0;
    let error = initialize(&mut MemoryStore::default(), &invalid).unwrap_err();
    assert_eq!(error.code, ErrorCode::SchemaInvalid);
}

#[test]
fn kept_list_and_removal_use_only_local_store() {
    let (_, kept) = fixtures();
    let mut store = MemoryStore::default();
    store.kept.insert(kept.repro_id, kept.clone());
    assert_eq!(list_kept(&store).unwrap(), vec![kept.clone()]);
    remove_kept(&mut store, kept.repro_id).unwrap();
    assert!(list_kept(&store).unwrap().is_empty());
    let error = remove_kept(&mut store, kept.repro_id).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}

fn fixtures() -> (TestProject, KeptReference) {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    let config = TestProject { revision: 1 };
    let kept = canonical::parse_strict(
        &serde_json::to_vec(&vectors["positive"]["kept_reference"]["value"]).unwrap(),
    )
    .unwrap();
    (config, kept)
}
