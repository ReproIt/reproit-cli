use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use reproit_app::{KeptReferenceStore as _, initialize};
use reproit_backend::config::ProjectConfig;
use reproit_cli::FilesystemRepository;
use reproit_core::{canonical, model::KeptReference};
use serde_json::Value;

const REPRO_ID: &str = "rpr_01890f3e-7b1c-7cc0-8a1b-123456789ac2";
const VECTORS: &str = reproit_core::contracts::PROTOCOL_VECTORS;
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_PROCESS_OUTPUT_BYTES: usize = 1024 * 1024;
const FORBIDDEN_DEFAULT_TERMS: [&str; 15] = [
    "admission",
    "attestation",
    "candidate",
    "capability set",
    "capsule",
    "capture batch",
    "closure",
    "digest",
    "executor",
    "manifest",
    "provider",
    "subject",
    "translator",
    "triage revision",
    "credential",
];

fn reproit() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reproit"))
}

fn run_at(root: &Path, arguments: &[&str], details: bool) -> std::process::Output {
    let mut command = reproit();
    command.current_dir(root).stdin(Stdio::null());
    command
        .env("CI", "true")
        .env("REPROIT_AUTHORITY", "https://fixture.reproit.test")
        .env_remove("REPROIT_CLI_CLIENT_ID")
        .env_remove("REPROIT_CLOUD_ORIGIN");
    if details {
        command.arg("--details");
    }
    command.args(arguments).output().expect("run CLI process")
}

fn assert_bounded(output: &std::process::Output) {
    let combined_bytes = output.stdout.len().saturating_add(output.stderr.len());
    assert!(combined_bytes <= MAX_PROCESS_OUTPUT_BYTES);
    for stream in [&output.stdout, &output.stderr] {
        for line in stream.split(|byte| *byte == b'\n') {
            assert!(line.len() <= MAX_LINE_BYTES);
        }
    }
}

fn assert_no_forbidden_default_terms(output: &std::process::Output) {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    for term in FORBIDDEN_DEFAULT_TERMS {
        assert!(!combined.contains(term), "default output exposed {term:?}");
    }
}

fn write_project_fixture(root: &Path, include_kept: bool) -> KeptReference {
    let vectors: Value = serde_json::from_str(VECTORS).expect("protocol vectors");
    let config: ProjectConfig = canonical::parse_strict(
        &serde_json::to_vec(&vectors["positive"]["project_config"]["value"])
            .expect("project fixture JSON"),
    )
    .expect("project fixture");
    let kept: KeptReference = canonical::parse_strict(
        &serde_json::to_vec(&vectors["positive"]["kept_reference"]["value"])
            .expect("kept fixture JSON"),
    )
    .expect("kept fixture");
    let mut repository = FilesystemRepository::new(root);
    initialize(&mut repository, &config).expect("write project fixture");
    if include_kept {
        repository
            .write_kept(&kept)
            .expect("write kept reference fixture");
    }
    kept
}

#[test]
fn help_is_a_process_level_pass() {
    let output = reproit().arg("--help").output().expect("run CLI help");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("Reproduce, fix, and keep production bugs."));
    assert!(output.stderr.is_empty());
}

#[test]
fn mcp_stdio_lists_the_seven_bounded_tools() {
    let mut child = reproit()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start MCP server");
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n"
    );
    child
        .stdin
        .take()
        .expect("MCP standard input")
        .write_all(input.as_bytes())
        .expect("write MCP requests");
    let output = child.wait_with_output().expect("wait for MCP server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert!(output.stdout.len() <= MAX_PROCESS_OUTPUT_BYTES);
    let responses = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("MCP JSON response"))
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("MCP tools");
    assert_eq!(tools.len(), 7);
    assert!(tools.iter().any(|tool| tool["name"] == "remove_repro"));
}

#[test]
fn public_command_surface_contains_the_nine_contract_commands() {
    let output = reproit().arg("--help").output().expect("run CLI help");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    for command in [
        "login", "init", "list", "triage", "debug", "check", "keep", "mcp", "remove",
    ] {
        assert!(help.contains(&format!("  {command}")), "missing {command}");
    }
    assert!(!help.contains("  link"));
    assert!(!help.contains("  help"));

    for arguments in [
        &["login", "--help"][..],
        &["init", "--help"],
        &["list", "--help"],
        &["triage", "--help"],
        &["debug", "--help"],
        &["check", "--help"],
        &["keep", "--help"],
        &["mcp", "--help"],
        &["remove", "--help"],
    ] {
        let output = reproit()
            .args(arguments)
            .output()
            .expect("run command help");
        assert_eq!(output.status.code(), Some(0), "arguments: {arguments:?}");
        assert!(output.stderr.is_empty(), "arguments: {arguments:?}");
        assert_bounded(&output);
    }

    let check_help = reproit()
        .args(["check", "--help"])
        .output()
        .expect("run check help");
    assert!(
        String::from_utf8(check_help.stdout)
            .expect("UTF-8 check help")
            .contains("Usage: reproit check [OPTIONS] [REPRO_ID]")
    );
}

#[test]
fn noncontract_commands_use_the_bounded_command_error() {
    for (arguments, name) in [
        (
            &["link", "oci://registry.example.com/team/repros"][..],
            "link",
        ),
        (&["help"][..], "help"),
    ] {
        let output = reproit()
            .args(arguments)
            .output()
            .expect("reject noncontract command");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr.clone()).expect("UTF-8 error"),
            format!("error: unrecognized command '{name}'\nRun 'reproit --help' for usage.\n")
        );
        assert_bounded(&output);
        assert_no_forbidden_default_terms(&output);
    }
}

#[test]
fn invalid_command_is_a_bounded_command_error() {
    let output = reproit()
        .arg("not-a-command")
        .output()
        .expect("run invalid CLI command");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("UTF-8 error"),
        "error: unrecognized command 'not-a-command'\nRun 'reproit --help' for usage.\n"
    );
}

#[test]
fn invalid_command_error_sanitizes_and_bounds_the_echoed_name() {
    let hostile = format!("evil\u{7}{}'quote", "a".repeat(200));
    let output = reproit()
        .arg(&hostile)
        .output()
        .expect("run hostile CLI command");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    let first_line = stderr.lines().next().expect("command error line");
    assert!(first_line.starts_with("error: unrecognized command '"));
    assert!(first_line.len() <= "error: unrecognized command ''".len() + 64);
    assert!(!stderr.contains('\u{7}'));
    assert_eq!(
        stderr.lines().nth(1),
        Some("Run 'reproit --help' for usage.")
    );
}

#[test]
fn invalid_flag_is_a_bounded_command_error() {
    let output = reproit()
        .args(["list", "--not-a-flag"])
        .output()
        .expect("run CLI command with an invalid flag");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("UTF-8 error"),
        "error: invalid command usage\nRun 'reproit --help' for usage.\n"
    );
}

/// A valid Repro command that fails during evaluation keeps the sanitized
/// Repro diagnostic. Command parsing failures never reach this path.
#[test]
fn valid_command_evaluation_failure_reports_the_repro_diagnostic() {
    let workspace = tempfile::tempdir().expect("create an empty CLI workspace");
    let output = reproit()
        .current_dir(workspace.path())
        .args(["remove", "rpr_01890f3e-7b1c-7cc0-8a1b-123456789abc"])
        .output()
        .expect("run remove without a kept reference");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("UTF-8 error"),
        "Repro It could not evaluate this Repro.\nRun again with --details.\n"
    );
}

#[test]
fn partial_login_override_uses_public_language_without_internal_details() {
    let output = reproit()
        .env("REPROIT_AUTHORITY", "https://fixture.reproit.test")
        .env_remove("REPROIT_CLI_CLIENT_ID")
        .arg("login")
        .output()
        .expect("run login with a partial override");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert_eq!(
        stderr,
        "Repro It could not evaluate this Repro.\nRun again with --details.\n"
    );
    for forbidden in ["CONFIG_CONFLICT", "authentication configuration", "digest"] {
        assert!(!stderr.contains(forbidden));
    }
}

#[test]
fn details_adds_code_without_changing_the_exit_code() {
    let output = reproit()
        .env("REPROIT_AUTHORITY", "https://fixture.reproit.test")
        .env_remove("REPROIT_CLI_CLIENT_ID")
        .args(["--details", "login"])
        .output()
        .expect("run detailed login with a partial override");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("Code: CONFIG_CONFLICT\n"));
    assert!(stderr.contains("Retryable: no\n"));
    assert!(!stderr.contains("authentication configuration"));
}

#[test]
fn source_build_without_official_oauth_metadata_reports_the_typed_blocker() {
    if option_env!("REPROIT_OFFICIAL_CLI_AUTHORITY").is_some()
        && option_env!("REPROIT_OFFICIAL_CLI_CLIENT_ID").is_some()
    {
        return;
    }
    let output = reproit()
        .env_remove("REPROIT_AUTHORITY")
        .env_remove("REPROIT_CLI_CLIENT_ID")
        .args(["--details", "login"])
        .output()
        .expect("run login from a source build without official metadata");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("Code: CONFIG_CONFLICT\n"));
    assert!(stderr.contains("Retryable: no\n"));
}

#[test]
fn default_errors_are_canonical_for_each_public_operation() {
    let temporary = tempfile::tempdir().expect("temporary repository");
    let generic = "Repro It could not evaluate this Repro.\nRun again with --details.\n";
    let source = concat!(
        "Repro It could not get the required source.\n",
        "Check your Git access, then try again.\n"
    );
    let cases = [
        (vec!["login"], "", generic),
        (
            vec![
                "init",
                "--non-interactive",
                "--service",
                "acme/orders",
                "--sdk",
                "rust",
                "--",
                "cargo",
                "run",
            ],
            "",
            source,
        ),
        (vec!["list"], "", generic),
        (vec!["triage", REPRO_ID], "", generic),
        (vec!["debug", REPRO_ID], "", generic),
        (
            vec!["check", REPRO_ID],
            "ERROR rpr_01890f3e-7b1c-7cc0-8a1b-123456789ac2\n",
            generic,
        ),
        (vec!["check"], "", generic),
        (vec!["keep", REPRO_ID], "", generic),
        (vec!["remove", REPRO_ID], "", generic),
    ];
    for (arguments, expected_stdout, expected_stderr) in cases {
        let output = run_at(temporary.path(), &arguments, false);
        assert_eq!(output.status.code(), Some(2), "arguments: {arguments:?}");
        assert_eq!(
            String::from_utf8(output.stdout.clone()).expect("UTF-8 output"),
            expected_stdout,
            "arguments: {arguments:?}"
        );
        assert_eq!(
            String::from_utf8(output.stderr.clone()).expect("UTF-8 error"),
            expected_stderr,
            "arguments: {arguments:?}"
        );
        assert_bounded(&output);
        assert_no_forbidden_default_terms(&output);
    }
}

#[test]
fn details_preserve_results_and_exit_codes_for_each_public_operation() {
    let temporary = tempfile::tempdir().expect("temporary repository");
    let cases = [
        (vec!["login"], "CONFIG_CONFLICT", false),
        (
            vec![
                "init",
                "--non-interactive",
                "--service",
                "acme/orders",
                "--sdk",
                "rust",
                "--",
                "cargo",
                "run",
            ],
            "SOURCE_CHECKOUT_FAILED",
            true,
        ),
        (vec!["list"], "EVALUATION_ERROR", false),
        (vec!["triage", REPRO_ID], "EVALUATION_ERROR", false),
        (vec!["debug", REPRO_ID], "EVALUATION_ERROR", false),
        (vec!["check", REPRO_ID], "EVALUATION_ERROR", false),
        (vec!["check"], "EVALUATION_ERROR", false),
        (vec!["keep", REPRO_ID], "EVALUATION_ERROR", false),
        (vec!["remove", REPRO_ID], "NOT_FOUND", false),
    ];
    for (arguments, code, retryable) in cases {
        let default = run_at(temporary.path(), &arguments, false);
        let detailed = run_at(temporary.path(), &arguments, true);
        assert_eq!(detailed.status.code(), default.status.code());
        assert_eq!(detailed.stdout, default.stdout);
        assert!(detailed.stderr.starts_with(&default.stderr));
        let detail_suffix = format!(
            "Code: {code}\nRetryable: {}\n",
            if retryable { "yes" } else { "no" }
        );
        assert!(
            detailed.stderr.ends_with(detail_suffix.as_bytes()),
            "arguments: {arguments:?}"
        );
        assert_bounded(&detailed);
    }
}

#[test]
fn non_interactive_init_never_prompts_on_standard_input() {
    let temporary = tempfile::tempdir().expect("temporary repository");
    let output = run_at(
        temporary.path(),
        &[
            "init",
            "--non-interactive",
            "--service",
            "acme/orders",
            "--sdk",
            "rust",
            "--",
            "cargo",
            "run",
        ],
        false,
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for prompt in [
        "Select a service:",
        "Select an SDK:",
        "Apply this change?",
        "[y/N]",
    ] {
        assert!(!combined.contains(prompt));
    }
}

#[test]
fn empty_kept_list_is_an_exact_process_level_success() {
    let temporary = tempfile::tempdir().expect("temporary repository");
    write_project_fixture(temporary.path(), false);
    let output = run_at(temporary.path(), &["list", "--kept"], false);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"No kept Repros.\n");
    assert!(output.stderr.is_empty());
    assert_bounded(&output);
    assert_no_forbidden_default_terms(&output);
}

#[test]
fn kept_list_and_remove_have_exact_process_level_success_output() {
    let temporary = tempfile::tempdir().expect("temporary repository");
    let kept = write_project_fixture(temporary.path(), true);

    let listed = run_at(temporary.path(), &["list", "--kept"], false);
    assert_eq!(listed.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(listed.stdout.clone()).expect("UTF-8 kept list"),
        format!(
            "{}\t.reproit/repros/{}.toml\n",
            kept.repro_id, kept.repro_id
        )
    );
    assert!(listed.stderr.is_empty());
    assert_bounded(&listed);
    assert_no_forbidden_default_terms(&listed);

    let removed = run_at(temporary.path(), &["remove", REPRO_ID], false);
    assert_eq!(removed.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(removed.stdout.clone()).expect("UTF-8 remove result"),
        concat!(
            "Removed the kept reference for rpr_01890f3e-7b1c-7cc0-8a1b-123456789ac2.\n",
            "Cloud history was not deleted.\n"
        )
    );
    assert!(removed.stderr.is_empty());
    assert_bounded(&removed);
    assert_no_forbidden_default_terms(&removed);

    let relisted = run_at(temporary.path(), &["list", "--kept"], false);
    assert_eq!(relisted.status.code(), Some(0));
    assert_eq!(relisted.stdout, b"No kept Repros.\n");
    assert!(relisted.stderr.is_empty());
}

#[test]
fn every_sdk_is_an_explicit_init_choice() {
    for sdk in ["dotnet", "go", "nodejs", "python", "rust"] {
        let output = reproit()
            .args(["init", "--help"])
            .output()
            .expect("run init help");
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert!(help.contains("--sdk <SDK>"));
        assert!(help.contains(sdk));
    }
}

#[test]
fn init_help_exposes_only_the_public_initialization_inputs() {
    let output = reproit()
        .args(["init", "--help"])
        .output()
        .expect("run init help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    for public in ["--non-interactive", "--service", "--sdk", "--service-path"] {
        assert!(help.contains(public));
    }
    for internal in [
        "--environment-policy",
        "--organization-id",
        "--project-id",
        "--repository-id",
        "--service-id",
        "--keep-destination",
        "--key-reference",
        "--working-directory",
    ] {
        assert!(!help.contains(internal));
    }
}

#[test]
fn init_rejects_the_excluded_private_policy_input() {
    let workspace = tempfile::tempdir().expect("create a non-repository workspace");
    let output = reproit()
        .current_dir(workspace.path())
        .args([
            "init",
            "--environment-policy",
            "policy.json",
            "--service",
            "acme/orders",
            "--sdk",
            "rust",
            "--",
            "cargo",
            "run",
        ])
        .output()
        .expect("reject an excluded private policy input");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr.clone()).expect("UTF-8 error");
    assert_eq!(
        stderr,
        "error: invalid command usage\nRun 'reproit --help' for usage.\n"
    );
    assert_bounded(&output);
    assert_no_forbidden_default_terms(&output);
}

#[test]
fn triage_help_uses_public_workflow_actions() {
    let output = reproit()
        .args(["triage", "--help"])
        .output()
        .expect("run triage help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(help.contains("--resolve"));
    assert!(help.contains("--reopen"));
    assert!(!help.contains("--workflow"));
}

#[test]
fn debug_is_always_the_standard_attach_workflow() {
    let output = reproit()
        .args(["debug", "--help"])
        .output()
        .expect("run debug help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(!help.contains("--attach"));
}
