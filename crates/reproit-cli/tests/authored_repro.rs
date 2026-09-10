#![cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]

use std::{
    fs,
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
    sync::{Mutex, MutexGuard},
};

use reproit_cli::authored_repro::{
    AddReproInput, AuthoredCheckOutcome, add_repro, add_repro_with_cancellation,
    authored_repro_ids, check_authored_repro,
};
use reproit_core::{ErrorCode, canonical, identity::Digest};
use reproit_experiments::CancellationFlag;
use serde_json::{Value, json};

static AUTHORED_TEST_LOCK: Mutex<()> = Mutex::new(());

fn authored_test_guard() -> MutexGuard<'static, ()> {
    AUTHORED_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn an_authored_definition_seals_and_checks() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let result = add_repro(workspace.root.path(), &input()).unwrap();

    assert_eq!(result.profile, "experiments");
    assert_eq!(result.observed_results.len(), 2);
    assert_eq!(result.observed_results[0].criterion_id, "latency");
    assert_eq!(result.observed_results[0].observed_median, 80);
    assert_eq!(result.observed_results[0].expected_minimum, 70);
    assert_eq!(result.observed_results[0].expected_maximum, 90);
    assert_eq!(
        authored_repro_ids(workspace.root.path()).unwrap(),
        [result.repro_id]
    );
    let check = check_authored_repro(workspace.root.path(), result.repro_id)
        .unwrap()
        .unwrap();
    assert_eq!(check.outcome, AuthoredCheckOutcome::Pass);
    assert_eq!(check.observed_results[0].observed_median, 80);
    let repro_root = workspace
        .root
        .path()
        .join(".reproit/authored-repros")
        .join(result.repro_id.to_string());
    for name in [
        "capsule.json",
        "claim.json",
        "definition.json",
        "initial-evidence.json",
        "initial-result.json",
        "reference.json",
    ] {
        let bytes = fs::read(repro_root.join(name)).unwrap();
        let value: Value = canonical::parse_strict(&bytes).unwrap();
        assert_eq!(canonical::canonical_bytes(&value).unwrap(), bytes);
    }
    assert!(!repro_root.join("workload").exists());
    assert!(!repro_root.join("workload.exe").exists());
    assert!(
        fs::read_dir(repro_root.join("objects/sha256"))
            .unwrap()
            .all(|entry| entry.unwrap().metadata().unwrap().len() < 1_024)
    );
    let capsule_text = fs::read_to_string(repro_root.join("capsule.json")).unwrap();
    assert!(!capsule_text.contains("executable"));
    let claim_bytes = fs::read(repro_root.join("claim.json")).unwrap();
    let claim: Value = canonical::parse_strict(&claim_bytes).unwrap();
    assert_eq!(Digest::of(&claim_bytes), result.claim_digest);
    assert_eq!(claim["criterion_id"], "latency");
    assert_eq!(claim["dataset_input_id"], "dataset");
    assert_eq!(
        claim["scope"],
        "The declared code evaluated on the declared dataset."
    );
    for excluded in [
        "commands",
        "environment",
        "expected",
        "inputs",
        "measurements",
        "observed",
    ] {
        assert!(claim.get(excluded).is_none());
    }
    let result: Value =
        canonical::parse_strict(&fs::read(repro_root.join("initial-result.json")).unwrap())
            .unwrap();
    assert_eq!(result["criteria"][0]["expected_minimum"], 70);
    assert_eq!(result["criteria"][0]["expected_maximum"], 90);
    assert_eq!(result["criteria"][0]["observed_minimum"], 80);
    assert_eq!(result["criteria"][0]["observed_median"], 80);
    assert_eq!(result["criteria"][0]["observed_maximum"], 80);
    let evidence: Value =
        canonical::parse_strict(&fs::read(repro_root.join("initial-evidence.json")).unwrap())
            .unwrap();
    assert_eq!(evidence["verified_inputs"][0]["id"], "code");
    assert_eq!(evidence["verified_inputs"][1]["id"], "dataset");
}

#[test]
fn an_authored_command_stops_descendants_before_reading_measurements() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let path = workspace.root.path().join("experiment.json");
    let mut definition: Value = canonical::parse_strict(&fs::read(&path).unwrap()).unwrap();
    definition["run"]["command"]["arguments"] = json!(["orphan"]);
    fs::write(path, canonical::canonical_bytes(&definition).unwrap()).unwrap();
    let added = add_repro(workspace.root.path(), &input()).unwrap();
    let checked = check_authored_repro(workspace.root.path(), added.repro_id)
        .unwrap()
        .unwrap();
    assert_eq!(checked.outcome, AuthoredCheckOutcome::Pass);
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn a_direct_cargo_run_definition_seals_and_checks() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    workspace.use_direct_cargo_run();

    let result = add_repro(workspace.root.path(), &input()).unwrap();

    assert_eq!(
        check_authored_repro(workspace.root.path(), result.repro_id)
            .unwrap()
            .unwrap()
            .outcome,
        AuthoredCheckOutcome::Pass
    );
}

#[test]
fn an_out_of_range_result_publishes_no_capsule() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("outside-range");
    let error = add_repro(workspace.root.path(), &input()).unwrap_err();

    assert_eq!(error.code, ErrorCode::DifferentFailure);
    assert!(
        authored_repro_ids(workspace.root.path())
            .unwrap()
            .is_empty()
    );
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn a_failed_setup_command_publishes_no_capsule() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    workspace.set_first_setup_arguments(json!(["not-a-git-subcommand"]));

    let error = add_repro(workspace.root.path(), &input()).unwrap_err();

    assert_eq!(error.code, ErrorCode::EvaluationError);
    assert!(
        authored_repro_ids(workspace.root.path())
            .unwrap()
            .is_empty()
    );
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn a_mismatched_input_publishes_no_capsule() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let path = workspace.root.path().join("experiment.json");
    let mut definition: Value = canonical::parse_strict(&fs::read(&path).unwrap()).unwrap();
    definition["inputs"][1]["digest"] = Value::String(format!("sha256:{}", "0".repeat(64)));
    fs::write(path, canonical::canonical_bytes(&definition).unwrap()).unwrap();

    let error = add_repro(workspace.root.path(), &input()).unwrap_err();

    assert_eq!(error.code, ErrorCode::EvaluationError);
    assert!(error.message.contains("UNKNOWN"));
    assert!(
        authored_repro_ids(workspace.root.path())
            .unwrap()
            .is_empty()
    );
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn changed_fetched_content_returns_unknown() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    workspace.use_moving_revision();
    let added = add_repro(workspace.root.path(), &input()).unwrap();
    workspace.commit_dataset(Some(b"different dataset\n"));

    let checked = check_authored_repro(workspace.root.path(), added.repro_id)
        .unwrap()
        .unwrap();

    assert_eq!(checked.outcome, AuthoredCheckOutcome::Unknown);
    assert!(checked.observed_results.is_empty());
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn a_missing_fetched_input_returns_unknown() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    workspace.use_moving_revision();
    let added = add_repro(workspace.root.path(), &input()).unwrap();
    workspace.commit_dataset(None);

    let checked = check_authored_repro(workspace.root.path(), added.repro_id)
        .unwrap()
        .unwrap();

    assert_eq!(checked.outcome, AuthoredCheckOutcome::Unknown);
    assert!(checked.observed_results.is_empty());
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn an_unauthorized_profile_fails_before_execution() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let definition_path = workspace.root.path().join("experiment.json");
    let mut definition: Value =
        canonical::parse_strict(&fs::read(&definition_path).unwrap()).unwrap();
    definition["profile"] = Value::String("security".to_owned());
    fs::write(
        &definition_path,
        canonical::canonical_bytes(&definition).unwrap(),
    )
    .unwrap();

    let error = add_repro(workspace.root.path(), &input()).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedCapabilitySet);
    assert!(
        authored_repro_ids(workspace.root.path())
            .unwrap()
            .is_empty()
    );
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn cancellation_publishes_no_capsule_or_staging_directory() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let cancellation = CancellationFlag::new();
    cancellation.cancel();

    let error =
        add_repro_with_cancellation(workspace.root.path(), &input(), &cancellation).unwrap_err();
    assert_eq!(error.code, ErrorCode::EvaluationError);
    assert!(
        authored_repro_ids(workspace.root.path())
            .unwrap()
            .is_empty()
    );
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn changed_capsule_bytes_fail_before_a_later_check() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let result = add_repro(workspace.root.path(), &input()).unwrap();
    let capsule = workspace
        .root
        .path()
        .join(".reproit/authored-repros")
        .join(result.repro_id.to_string())
        .join("capsule.json");
    let mut bytes = fs::read(&capsule).unwrap();
    bytes.push(b'\n');
    fs::write(capsule, bytes).unwrap();

    assert!(check_authored_repro(workspace.root.path(), result.repro_id).is_err());
    assert_no_staging_directories(workspace.root.path());
}

#[test]
fn missing_sealed_evidence_returns_unknown() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let result = add_repro(workspace.root.path(), &input()).unwrap();
    let evidence = workspace
        .root
        .path()
        .join(".reproit/authored-repros")
        .join(result.repro_id.to_string())
        .join("initial-evidence.json");
    fs::remove_file(evidence).unwrap();

    let checked = check_authored_repro(workspace.root.path(), result.repro_id)
        .unwrap()
        .unwrap();
    assert_eq!(checked.outcome, AuthoredCheckOutcome::Unknown);
    assert!(checked.observed_results.is_empty());
}

#[test]
fn changed_claim_bytes_return_an_error() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    let result = add_repro(workspace.root.path(), &input()).unwrap();
    let claim = workspace
        .root
        .path()
        .join(".reproit/authored-repros")
        .join(result.repro_id.to_string())
        .join("claim.json");
    let mut bytes = fs::read(&claim).unwrap();
    bytes.push(b'\n');
    fs::write(claim, bytes).unwrap();

    assert!(check_authored_repro(workspace.root.path(), result.repro_id).is_err());
}

#[test]
fn cli_and_mcp_add_the_same_repro_and_cli_checks_it() {
    let _guard = authored_test_guard();
    let cli_workspace = Workspace::new("candidate");
    let cli_add = reproit(cli_workspace.root.path())
        .args(["add", "experiment.json"])
        .output()
        .unwrap();
    assert!(cli_add.status.success());
    assert!(cli_add.stderr.is_empty());
    let cli_stdout = String::from_utf8(cli_add.stdout).unwrap();
    assert!(
        cli_stdout
            .contains("Observed latency: 80 to 80 microseconds. Median: 80. Expected: 70 to 90.")
    );
    let cli_repro_id = cli_stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Added "))
        .and_then(|value| value.strip_suffix('.'))
        .unwrap();
    let cli_claim_digest = cli_stdout
        .lines()
        .find_map(|line| line.strip_prefix("Claim "))
        .and_then(|value| value.strip_suffix('.'))
        .unwrap();

    let check = reproit(cli_workspace.root.path())
        .args(["check", cli_repro_id])
        .output()
        .unwrap();
    assert!(check.status.success());
    let check_stdout = String::from_utf8(check.stdout).unwrap();
    assert!(check_stdout.starts_with(&format!("PASS {cli_repro_id}\n")));
    assert!(
        check_stdout
            .contains("Observed latency: 80 to 80 microseconds. Median: 80. Expected: 70 to 90.")
    );
    assert!(check.stderr.is_empty());

    fs::remove_dir_all(
        cli_workspace
            .root
            .path()
            .join(".reproit/authored-repros")
            .join(cli_repro_id),
    )
    .unwrap();
    let mut child = reproit(cli_workspace.root.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"add_repro\",",
        "\"arguments\":{\"definition_path\":\"experiment.json\"}}}\n"
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(requests.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let responses = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[1]["result"]["structuredContent"]["repro_id"],
        cli_repro_id
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["next_command"],
        format!("reproit check {cli_repro_id}")
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["claim_digest"],
        cli_claim_digest
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["observed_results"][0]["observed_median"],
        80
    );
}

#[test]
fn mcp_cancellation_publishes_no_capsule_or_staging_directory() {
    let _guard = authored_test_guard();
    let workspace = Workspace::new("candidate");
    workspace.set_run_arguments(json!(["wait"]));
    let mut child = reproit(workspace.root.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"add_repro\",",
        "\"arguments\":{\"definition_path\":\"experiment.json\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",",
        "\"params\":{\"requestId\":2}}\n"
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(requests.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let responses = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 1);
    assert!(
        authored_repro_ids(workspace.root.path())
            .unwrap()
            .is_empty()
    );
    assert_no_staging_directories(workspace.root.path());
}

fn input() -> AddReproInput {
    AddReproInput {
        definition_path: "experiment.json".to_owned(),
    }
}

fn assert_no_staging_directories(root: &Path) {
    for entry in fs::read_dir(root.join(".reproit")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(!name.starts_with("authored-repro-"));
        assert!(!name.starts_with("authored-check-"));
    }
}

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    fn new(candidate_name: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "--quiet"]);
        git(root.path(), &["config", "user.email", "tests@reproit.dev"]);
        git(root.path(), &["config", "user.name", "Repro It Tests"]);
        git(
            root.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/research/workload.git",
            ],
        );
        let observed_value = if candidate_name == "candidate" {
            80
        } else {
            100
        };
        fs::write(root.path().join(".gitattributes"), b"* -text\n").unwrap();
        write_workload(root.path(), observed_value);
        fs::write(root.path().join("data.txt"), b"research dataset\n").unwrap();
        git(
            root.path(),
            &[
                "add",
                ".gitattributes",
                "Cargo.toml",
                "data.txt",
                "src/main.rs",
            ],
        );
        git(root.path(), &["commit", "--quiet", "-m", "research"]);
        let revision = git_output(root.path(), &["rev-parse", "HEAD"]);
        fs::create_dir(root.path().join(".reproit")).unwrap();
        fs::write(
            root.path().join(".reproit/project.toml"),
            b"fixture = true\n",
        )
        .unwrap();
        let repository_path = root.path().to_string_lossy().into_owned();
        let code_digest = directory_digest(&[(
            "main.rs",
            &fs::read(root.path().join("src/main.rs")).unwrap(),
        )]);
        let dataset_digest = Digest::of(&fs::read(root.path().join("data.txt")).unwrap());
        let definition =
            authored_definition(&repository_path, &revision, code_digest, dataset_digest);
        fs::write(
            root.path().join("experiment.json"),
            canonical::canonical_bytes(&definition).unwrap(),
        )
        .unwrap();
        Self { root }
    }

    fn set_run_arguments(&self, arguments: Value) {
        let path = self.root.path().join("experiment.json");
        let mut definition: Value = canonical::parse_strict(&fs::read(&path).unwrap()).unwrap();
        definition["run"]["command"]["arguments"] = arguments;
        fs::write(path, canonical::canonical_bytes(&definition).unwrap()).unwrap();
    }

    fn set_first_setup_arguments(&self, arguments: Value) {
        let path = self.root.path().join("experiment.json");
        let mut definition: Value = canonical::parse_strict(&fs::read(&path).unwrap()).unwrap();
        definition["setup"][0]["arguments"] = arguments;
        fs::write(path, canonical::canonical_bytes(&definition).unwrap()).unwrap();
    }

    fn use_direct_cargo_run(&self) {
        let path = self.root.path().join("experiment.json");
        let mut definition: Value = canonical::parse_strict(&fs::read(&path).unwrap()).unwrap();
        definition["setup"].as_array_mut().unwrap().pop();
        definition["run"]["command"] = json!({
            "arguments": ["run", "--quiet"],
            "program": "cargo",
            "working_directory": "research"
        });
        fs::write(path, canonical::canonical_bytes(&definition).unwrap()).unwrap();
    }

    fn use_moving_revision(&self) {
        let path = self.root.path().join("experiment.json");
        let mut definition: Value = canonical::parse_strict(&fs::read(&path).unwrap()).unwrap();
        definition["setup"].as_array_mut().unwrap().remove(1);
        fs::write(path, canonical::canonical_bytes(&definition).unwrap()).unwrap();
    }

    fn commit_dataset(&self, contents: Option<&[u8]>) {
        let path = self.root.path().join("data.txt");
        match contents {
            Some(contents) => fs::write(path, contents).unwrap(),
            None => fs::remove_file(path).unwrap(),
        }
        git(self.root.path(), &["add", "--all", "data.txt"]);
        git(
            self.root.path(),
            &["commit", "--quiet", "-m", "change dataset"],
        );
    }
}

fn authored_definition(
    repository_path: &str,
    revision: &str,
    code_digest: Digest,
    dataset_digest: Digest,
) -> Value {
    json!({
        "claim": {
            "criterion_id": "latency",
            "dataset_input_id": "dataset",
            "scope": "The declared code evaluated on the declared dataset."
        },
        "criteria": [{
            "expected": {"maximum": 90, "minimum": 70},
            "id": "latency",
            "unit": "microseconds"
        }, {
            "expected": {"maximum": 1100, "minimum": 900},
            "id": "throughput",
            "unit": "items_per_second"
        }],
        "format": "reproit.research-recipe.v1",
        "inputs": [{
            "digest": code_digest,
            "id": "code",
            "path": "research/src",
            "role": "code"
        }, {
            "digest": dataset_digest,
            "id": "dataset",
            "path": "research/data.txt",
            "role": "dataset"
        }],
        "profile": "experiments",
        "run": {
            "command": {
                "arguments": [],
                "program": "./workload{exe}",
                "working_directory": "research"
            },
            "measured_runs": 3
        },
        "setup": [{
            "arguments": ["clone", "--quiet", repository_path, "research"],
            "program": "git"
        }, {
            "arguments": ["checkout", "--quiet", "--detach", revision],
            "program": "git",
            "working_directory": "research"
        }, {
            "arguments": ["--edition=2024", "src/main.rs", "-o", "workload{exe}"],
            "program": "rustc",
            "working_directory": "research"
        }]
    })
}

fn directory_digest(entries: &[(&str, &[u8])]) -> Digest {
    let entries = entries
        .iter()
        .map(|(path, bytes)| {
            json!({
                "digest": Digest::of(bytes),
                "path": path,
                "size_bytes": bytes.len()
            })
        })
        .collect::<Vec<_>>();
    canonical::digest(&json!({
        "entries": entries,
        "format": "reproit.input-directory.v1"
    }))
    .unwrap()
}

fn write_workload(root: &Path, value: u64) {
    let source = format!(
        r##"use std::{{process::ExitCode, time::Duration}};

fn main() -> ExitCode {{
    if std::env::args().nth(1).as_deref() == Some("orphan") {{
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("wait").spawn().unwrap();
        std::thread::spawn(move || child.wait());
    }}
    if std::env::args().nth(1).as_deref() == Some("wait") {{
        std::thread::sleep(Duration::from_secs(5));
    }}
    println!(r#"{{{{"criterion_id":"latency","value":{value}}}}}"#);
    println!(r#"{{{{"criterion_id":"throughput","value":1000}}}}"#);
    ExitCode::SUCCESS
}}
"##
    );
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), source).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        b"[package]\nname = \"reproit-authored-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
}

fn git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn reproit(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reproit"));
    command.current_dir(root);
    command
}
