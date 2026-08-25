use std::{fs, str::FromStr};

use reproit_app::{KeptReferenceStore, initialize, list_kept, remove_kept};
use reproit_backend::config::ProjectConfig;
use reproit_cli::FilesystemRepository;
use reproit_core::{canonical, identity::ReproId, model::KeptReference};
use serde_json::Value;

const VECTORS: &str = reproit_core::contracts::PROTOCOL_VECTORS;

#[test]
fn filesystem_store_writes_only_tracked_configuration() {
    let temporary = tempfile::tempdir().unwrap();
    let (config, kept) = fixtures();
    let mut store = FilesystemRepository::new(temporary.path());
    initialize(&mut store, &config).unwrap();

    let config_text = fs::read_to_string(temporary.path().join(".reproit/project.toml")).unwrap();
    assert_eq!(
        toml::from_str::<ProjectConfig>(&config_text).unwrap(),
        config
    );

    store.write_kept(&kept).unwrap();
    store.write_kept(&kept).unwrap();
    assert_eq!(list_kept(&store).unwrap(), vec![kept.clone()]);

    let mut conflict = kept.clone();
    conflict.key_reference = "portable-key:org_01890f3e-7b1c-7cc0-8a1b-123456789abd:2".to_owned();
    let error = store.write_kept(&conflict).unwrap_err();
    assert_eq!(error.code, reproit_core::ErrorCode::ConfigConflict);
    remove_kept(&mut store, kept.repro_id).unwrap();
    assert!(list_kept(&store).unwrap().is_empty());
}

#[test]
fn reference_removal_never_uses_untrusted_input_as_a_path() {
    let temporary = tempfile::tempdir().unwrap();
    let mut store = FilesystemRepository::new(temporary.path());
    let missing = ReproId::from_str("rpr_01890f3e-7b1c-7cc0-8a1b-123456789ac2").unwrap();
    assert!(!store.remove_kept(missing).unwrap());
    assert!(temporary.path().exists());
}

fn fixtures() -> (ProjectConfig, KeptReference) {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    let config = canonical::parse_strict(
        &serde_json::to_vec(&vectors["positive"]["project_config"]["value"]).unwrap(),
    )
    .unwrap();
    let kept = canonical::parse_strict(
        &serde_json::to_vec(&vectors["positive"]["kept_reference"]["value"]).unwrap(),
    )
    .unwrap();
    (config, kept)
}
