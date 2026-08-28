use std::{fs, path::Path, process::Command};

use serde::Serialize;
use serde_json::{Value, json};

fn reproit() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reproit"))
}

#[derive(Serialize)]
struct Config<'a> {
    format: &'static str,
    suite_path: &'static str,
    bundle_path: &'static str,
    limits: Limits,
    baseline: Workload<'a>,
    candidate: Workload<'a>,
}

#[derive(Serialize)]
struct Limits {
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

#[derive(Serialize)]
struct Workload<'a> {
    executable: &'a str,
    arguments: [&'a str; 1],
    model_path: &'a str,
}

#[test]
fn gate_and_offline_verification_pass_for_matching_outputs() {
    let temporary = tempfile::tempdir().expect("create fixture directory");
    write_fixture(temporary.path(), "pass");

    let gate = reproit()
        .current_dir(temporary.path())
        .args(["gate", "--config", "release.toml"])
        .output()
        .expect("run the release gate");
    assert_eq!(gate.status.code(), Some(0));
    assert_eq!(gate.stdout, b"PASS\n");
    assert!(gate.stderr.is_empty());

    let verify = reproit()
        .current_dir(temporary.path())
        .args(["verify", "evidence.json"])
        .output()
        .expect("verify the evidence bundle");
    assert_eq!(verify.status.code(), Some(0));
    assert_eq!(verify.stdout, b"PASS\n");
    assert!(verify.stderr.is_empty());
}

#[test]
fn gate_blocks_regression_and_unknown_process_results() {
    for (mode, expected, exit_code) in [
        ("regression", "REGRESSION\n", 1),
        ("invalid", "UNKNOWN\n", 2),
    ] {
        let temporary = tempfile::tempdir().expect("create fixture directory");
        write_fixture(temporary.path(), mode);
        let output = reproit()
            .current_dir(temporary.path())
            .args(["gate", "--config", "release.toml"])
            .output()
            .expect("run the release gate");
        assert_eq!(output.status.code(), Some(exit_code));
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn offline_verification_rejects_changed_raw_evidence() {
    let temporary = tempfile::tempdir().expect("create fixture directory");
    write_fixture(temporary.path(), "pass");
    let gate = reproit()
        .current_dir(temporary.path())
        .args(["gate", "--config", "release.toml"])
        .output()
        .expect("run the release gate");
    assert_eq!(gate.status.code(), Some(0));

    let bundle_path = temporary.path().join("evidence.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    bundle["content"]["candidate"]["stdout_base64"] = Value::String("dGFtcGVyZWQ".to_owned());
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let verify = reproit()
        .current_dir(temporary.path())
        .args(["verify", "evidence.json"])
        .output()
        .expect("verify the changed evidence bundle");
    assert_eq!(verify.status.code(), Some(2));
    assert!(verify.stdout.is_empty());
    assert_eq!(
        String::from_utf8(verify.stderr).unwrap(),
        concat!(
            "The evidence bundle failed verification.\n",
            "Restore the evidence bundle from a verified copy.\n"
        )
    );
}

#[test]
fn offline_verification_rejects_incomplete_failure_observations() {
    let temporary = tempfile::tempdir().expect("create fixture directory");
    write_fixture(temporary.path(), "invalid");
    let gate = reproit()
        .current_dir(temporary.path())
        .args(["gate", "--config", "release.toml"])
        .output()
        .expect("run the release gate");
    assert_eq!(gate.status.code(), Some(2));

    let bundle_path = temporary.path().join("evidence.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    bundle["content"]["candidate"]["model_run"]["observations"] = json!([]);
    let candidate_run = &bundle["content"]["candidate"]["model_run"];
    bundle["content"]["bindings"]["candidate_model_run"] = Value::String(
        reproit_core::canonical::digest(candidate_run)
            .expect("digest changed candidate run")
            .to_string(),
    );
    bundle["content_digest"] = Value::String(
        reproit_core::canonical::digest(&bundle["content"])
            .expect("digest changed content")
            .to_string(),
    );
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let verify = reproit()
        .current_dir(temporary.path())
        .args(["verify", "evidence.json"])
        .output()
        .expect("verify incomplete failure observations");
    assert_eq!(verify.status.code(), Some(2));
}

#[test]
fn gate_rejects_input_and_output_paths_that_escape_the_config_root() {
    for field in ["suite_path", "model_path", "bundle_path"] {
        let temporary = tempfile::tempdir().expect("create fixture directory");
        write_fixture(temporary.path(), "pass");
        let config_path = temporary.path().join("release.toml");
        let config = fs::read_to_string(&config_path).unwrap();
        let changed = match field {
            "suite_path" => config.replacen("suite.json", "../suite.json", 1),
            "model_path" => config.replacen("baseline.json", "../baseline.json", 1),
            "bundle_path" => config.replacen("evidence.json", "../evidence.json", 1),
            _ => unreachable!(),
        };
        fs::write(&config_path, changed).unwrap();

        let output = reproit()
            .current_dir(temporary.path())
            .args(["gate", "--config", "release.toml"])
            .output()
            .expect("reject the release-gate path");
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, b"UNKNOWN\n");
    }
}

fn write_fixture(root: &Path, candidate_mode: &str) {
    let zero_digest = format!("sha256:{}", "0".repeat(64));
    let suite = json!({
        "cases": [{
            "case_id": "configured-color",
            "category": "model-migration",
            "criteria": [{
                "kind": "exact",
                "criterion_id": "stable-color",
                "expected_text": "blue"
            }],
            "input_text": "State the configured color."
        }],
        "format": "reproit.ml-evaluation-suite.v1",
        "world_id": zero_digest
    });
    fs::write(
        root.join("suite.json"),
        serde_json::to_vec_pretty(&suite).unwrap(),
    )
    .unwrap();
    write_model(root, "baseline.json", "baseline-model");
    write_model(root, "candidate.json", "candidate-model");

    let executable = env!("CARGO_BIN_EXE_reproit-release-reference");
    let config = Config {
        format: "reproit.release-gate-config.v1",
        suite_path: "suite.json",
        bundle_path: "evidence.json",
        limits: Limits {
            execution_seconds: 5,
            records: 8,
            stderr_bytes: 4_096,
            stdin_bytes: 16_384,
            stdout_bytes: 16_384,
        },
        baseline: Workload {
            executable,
            arguments: ["pass"],
            model_path: "baseline.json",
        },
        candidate: Workload {
            executable,
            arguments: [candidate_mode],
            model_path: "candidate.json",
        },
    };
    fs::write(
        root.join("release.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
}

fn write_model(root: &Path, file_name: &str, model_id: &str) {
    let zero_digest = format!("sha256:{}", "0".repeat(64));
    let model = json!({
        "configuration_digest": zero_digest,
        "format": "reproit.ml-model-identity.v1",
        "model_id": model_id,
        "revision": "revision-1",
        "weights_digest": zero_digest
    });
    fs::write(
        root.join(file_name),
        serde_json::to_vec_pretty(&model).unwrap(),
    )
    .unwrap();
}
