use super::*;

fn fuzz_campaign_grant() -> FuzzCampaignGrant {
    FuzzCampaignGrant {
        campaign_id: "fc_01890f3e-7b1c-7cc0-8a1b-123456789abc".parse().unwrap(),
        expires_at: "2026-08-30T00:00:00.000Z".parse().unwrap(),
        format: reproit_cloud_api::FuzzCampaignGrantFormat::V1,
        grant: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
        project_id: "prj_01890f3e-7b1e-7cc0-8a1b-123456789abc".parse().unwrap(),
        service_id: "svc_01890f3e-7b1f-7cc0-8a1b-123456789abc".parse().unwrap(),
    }
}

#[test]
fn campaign_grant_state_is_private_canonical_and_exclusive() {
    let directory = tempfile::tempdir().unwrap();
    let grant = fuzz_campaign_grant();
    let path = campaign_grant_state_path_from_home(directory.path(), grant.campaign_id);

    write_campaign_grant_state(&path, &grant).unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        canonical::canonical_bytes(&grant).unwrap()
    );
    assert!(write_campaign_grant_state(&path, &grant).is_err());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn campaign_validator_process_has_a_fixed_deadline() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("blocking-fuzzer");
    fs::write(&program, b"#!/bin/sh\nsleep 5\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();

    let error = run_bounded_fuzzer_command(
        &program,
        "validate",
        Path::new("campaign.toml"),
        Duration::from_millis(50),
    )
    .unwrap_err();

    assert_eq!(error.code, ErrorCode::RuntimeQuota);
}

#[test]
fn interactive_run_parser_preserves_bounded_arguments() {
    assert_eq!(
        split_run_answer("cargo run -- 'two words'").unwrap(),
        ["cargo", "run", "--", "two words"]
    );
    assert!(split_run_answer("cargo 'unterminated").is_err());
}

#[test]
fn initialization_requires_a_service_path_only_below_the_repository_root() {
    let mut arguments = InitArgs {
        non_interactive: true,
        run: vec!["app".to_owned()],
        sdk: Some(SdkArg::Rust),
        service: Some("acme/commerce/payments".to_owned()),
        service_path: None,
    };
    assert_eq!(select_service_path(&arguments, None, ".").unwrap(), ".");
    assert!(select_service_path(&arguments, None, "services/payments").is_err());
    arguments.service_path = Some("services/payments".to_owned());
    assert_eq!(
        select_service_path(&arguments, None, "services/payments").unwrap(),
        "services/payments"
    );
}

#[test]
fn interactive_and_non_interactive_init_share_node_run_normalization() {
    for non_interactive in [false, true] {
        let arguments = InitArgs {
            non_interactive,
            run: vec!["node".to_owned(), "service.mjs".to_owned()],
            sdk: Some(SdkArg::Nodejs),
            service: Some("acme/commerce/payments".to_owned()),
            service_path: None,
        };
        let run = select_startup_run(BackendSdk::Nodejs, &arguments, None, ".").unwrap();
        assert_eq!(
            run.arguments,
            ["--import", "@reproit/sdk/register", "service.mjs"]
        );
    }
}

#[test]
fn interactive_and_non_interactive_init_share_python_run_normalization() {
    for non_interactive in [false, true] {
        for run in [
            vec!["python3", "service.py", "--port", "8080"],
            vec!["python", "-m", "orders.worker", "--queue", "urgent"],
        ] {
            let arguments = InitArgs {
                non_interactive,
                run: run.iter().map(|value| (*value).to_owned()).collect(),
                sdk: Some(SdkArg::Python),
                service: Some("acme/commerce/payments".to_owned()),
                service_path: None,
            };
            let normalized = select_startup_run(BackendSdk::Python, &arguments, None, ".").unwrap();
            assert_eq!(
                &normalized.arguments[..3],
                ["-m", "reproit_sdk.register", "--"]
            );
            assert_eq!(&normalized.arguments[3..], &arguments.run[1..]);
        }
    }
}

#[test]
fn regression_exit_one_is_exclusive_to_check() {
    let error = Error::new(ErrorCode::DifferentFailure, "safe test error");
    assert_eq!(error_exit_code(PublicErrorContext::Check, &error), 1);
    assert_eq!(error_exit_code(PublicErrorContext::Source, &error), 2);
    assert_eq!(error_exit_code(PublicErrorContext::General, &error), 2);
}

#[test]
fn every_supported_sdk_setup_is_an_exact_release_without_generated_source() {
    let cases = [
        (
            BackendSdk::Dotnet,
            "dotnet add package ReproIt.Sdk --version 1.0.0",
        ),
        (BackendSdk::Go, "go get reproit.dev/sdk-go@v1.0.0"),
        (BackendSdk::Nodejs, "npm install @reproit/sdk@1.0.0"),
        (
            BackendSdk::Python,
            "python -m pip install reproit-sdk==1.0.0",
        ),
        (BackendSdk::Rust, "cargo add reproit-sdk-rust@1.0.0"),
    ];
    for (sdk, install_command) in cases {
        let setup = sdk_setup_lines(released_sdk(sdk).unwrap());
        assert_eq!(
            setup,
            [
                "Install the released SDK:",
                install_command,
                "Set REPROIT_MANAGED_PROJECT_TOKEN in your deployment secret store.",
                "Do not put the token in .reproit/project.toml.",
                "The SDK reads the token only after it captures a complete Failure.",
                "The SDK captures supported application observations automatically.",
                "Unsupported effects keep that Failure local.",
                "The SDK loads .reproit/project.toml and the current Git revision.",
            ]
        );
        let output = setup.join("\n");
        for generated_source in [
            "ReproIt.init",
            "ReproItCapture.Init",
            "operation_async",
            "reproit.Operation",
            "reproit.operation",
            "reproit_sdk_rust",
        ] {
            assert!(!output.contains(generated_source));
        }
    }
}

#[test]
fn authentication_and_authorization_have_distinct_public_actions() {
    assert_eq!(
        reproit_cli::render::public_error(
            PublicErrorContext::General,
            ErrorCode::AuthenticationRequired,
        ),
        ("Login is required.", "Run reproit login.")
    );
    assert_eq!(
        reproit_cli::render::public_error(
            PublicErrorContext::General,
            ErrorCode::AuthorizationDenied,
        ),
        (
            "You do not have access to this action.",
            "Ask an organization administrator for access."
        )
    );
}

#[test]
fn official_login_configuration_needs_no_runtime_override() {
    let configuration = select_login_configuration(
        None,
        None,
        Some("https://auth.reproit.test"),
        Some("client_reproit_public"),
    )
    .unwrap();
    assert_eq!(configuration.authority, "https://auth.reproit.test");
    assert_eq!(configuration.client_id, "client_reproit_public");
}

#[test]
fn login_configuration_accepts_only_a_complete_bounded_override() {
    let configuration = select_login_configuration(
        Some("https://fixture.reproit.test"),
        Some("reproit-cli-local"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(configuration.authority, "https://fixture.reproit.test");
    assert_eq!(configuration.client_id, "reproit-cli-local");

    assert!(
        select_login_configuration(
            Some("https://fixture.reproit.test"),
            None,
            Some("https://auth.reproit.test"),
            Some("client_reproit_public"),
        )
        .is_err()
    );
    assert!(
        select_login_configuration(
            None,
            Some("reproit-cli-local"),
            Some("https://auth.reproit.test"),
            Some("client_reproit_public"),
        )
        .is_err()
    );
    assert!(
        select_login_configuration(
            Some("https://fixture.reproit.test"),
            Some("invalid client"),
            None,
            None,
        )
        .is_err()
    );
    assert!(select_login_configuration(None, None, None, None).is_err());
    assert!(
        select_login_configuration(None, None, Some("https://auth.reproit.test"), None).is_err()
    );
    assert!(select_login_configuration(None, None, None, Some("client_reproit_public")).is_err());
}
