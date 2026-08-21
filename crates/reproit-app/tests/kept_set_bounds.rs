use reproit_app::{KeptReferenceStore, MAX_KEPT_REFERENCES, list_kept};
use reproit_core::{
    Error, ErrorCode,
    identity::{Digest, ReproId},
    model::{KeptReference, ProcessingMode},
};

struct Store(Vec<KeptReference>);

impl KeptReferenceStore for Store {
    fn list_kept(&self, limit: usize) -> Result<Vec<KeptReference>, Error> {
        Ok(self.0.iter().take(limit).cloned().collect())
    }

    fn remove_kept(&mut self, _repro_id: ReproId) -> Result<bool, Error> {
        Ok(false)
    }

    fn write_kept(&mut self, _reference: &KeptReference) -> Result<(), Error> {
        Ok(())
    }
}

fn reference(index: u128) -> KeptReference {
    let repro_id = format!("rpr_01890f3e-7b1c-7cc0-8a1b-{index:012x}")
        .parse()
        .expect("valid Repro identity");
    let capture_id = format!("cap_01890f3e-7b1c-7cc0-8a1b-{index:012x}")
        .parse()
        .expect("valid capture identity");
    let digest = Digest::of(&index.to_be_bytes());
    KeptReference {
        capsule_digest: digest,
        capture_batch: format!("oci-layout://keep/@{digest}"),
        capture_batch_digest: digest,
        capture_id,
        format: 1,
        key_reference: "portable:test-key".to_owned(),
        processing_mode: ProcessingMode::Private,
        profile: "backend".to_owned(),
        profile_format: 1,
        repro_id,
    }
}

#[test]
fn complete_kept_set_bound_is_accepted() {
    let references = (1..=MAX_KEPT_REFERENCES as u128).map(reference).collect();
    assert_eq!(
        list_kept(&Store(references)).unwrap().len(),
        MAX_KEPT_REFERENCES
    );
}

#[test]
fn one_reference_over_the_bound_is_rejected() {
    let references = (1..=MAX_KEPT_REFERENCES as u128 + 1)
        .map(reference)
        .collect();
    let error = list_kept(&Store(references)).unwrap_err();
    assert_eq!(error.code, ErrorCode::RuntimeQuota);
}

#[test]
fn duplicate_decoded_repro_identity_is_rejected() {
    let duplicate = reference(1);
    let error = list_kept(&Store(vec![duplicate.clone(), duplicate])).unwrap_err();
    assert_eq!(error.code, ErrorCode::ConfigConflict);
}
