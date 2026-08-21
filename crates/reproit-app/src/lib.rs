#![forbid(unsafe_code)]

pub mod agent;

use std::collections::BTreeSet;

use reproit_core::{
    Error, ErrorCode,
    identity::ReproId,
    model::{KeptReference, Validate},
};

pub use reproit_core::limits::MAX_KEPT_REFERENCES;

pub trait KeptReferenceStore {
    fn list_kept(&self, limit: usize) -> Result<Vec<KeptReference>, Error>;
    fn remove_kept(&mut self, repro_id: ReproId) -> Result<bool, Error>;
    fn write_kept(&mut self, reference: &KeptReference) -> Result<(), Error>;
}

pub trait ProjectStore<Project> {
    fn is_initialized(&self) -> Result<bool, Error>;
    fn read_project(&self) -> Result<Option<Project>, Error>;
    fn write_project(&mut self, config: &Project) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum InitializationResult {
    Created,
    Unchanged,
    Updated,
}

pub fn initialize<Project: Eq + Validate>(
    store: &mut impl ProjectStore<Project>,
    config: &Project,
) -> Result<InitializationResult, Error> {
    config.validate()?;
    match store.read_project()? {
        Some(current) if current == *config => Ok(InitializationResult::Unchanged),
        Some(_) => {
            store.write_project(config)?;
            Ok(InitializationResult::Updated)
        }
        None => {
            store.write_project(config)?;
            Ok(InitializationResult::Created)
        }
    }
}

pub fn list_kept(store: &impl KeptReferenceStore) -> Result<Vec<KeptReference>, Error> {
    let references = store.list_kept(MAX_KEPT_REFERENCES + 1)?;
    if references.len() > MAX_KEPT_REFERENCES {
        return Err(Error::new(
            ErrorCode::RuntimeQuota,
            "The kept Repro set exceeds its configured limit.",
        ));
    }
    let mut repro_ids = BTreeSet::new();
    for reference in &references {
        reference.validate()?;
        if !repro_ids.insert(reference.repro_id) {
            return Err(Error::new(
                ErrorCode::ConfigConflict,
                "More than one kept reference has the same Repro identity.",
            ));
        }
    }
    Ok(references)
}

pub fn remove_kept(store: &mut impl KeptReferenceStore, repro_id: ReproId) -> Result<(), Error> {
    if store.remove_kept(repro_id)? {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::NotFound,
        "The kept Repro reference was not found.",
    ))
}
