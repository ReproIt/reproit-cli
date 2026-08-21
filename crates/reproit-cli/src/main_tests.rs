use super::*;

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
fn regression_exit_one_is_exclusive_to_check() {
    let error = Error::new(ErrorCode::DifferentFailure, "safe test error");
    assert_eq!(error_exit_code(PublicErrorContext::Check, &error), 1);
    assert_eq!(error_exit_code(PublicErrorContext::Source, &error), 2);
    assert_eq!(error_exit_code(PublicErrorContext::General, &error), 2);
}

#[test]
fn every_sdk_setup_uses_local_artifacts_and_exact_public_boundaries() {
    let cases = [
        (
            BackendSdk::Dotnet,
            "ReproIt.Sdk",
            "ManagedProjectToken",
            "Operations.Run",
        ),
        (
            BackendSdk::Go,
            "reproit.dev/sdk-go",
            "NewManagedProjectToken",
            "RunOperation",
        ),
        (
            BackendSdk::Nodejs,
            "reproit-sdk-1.0.0.tgz",
            "ManagedProjectToken",
            "runOperation",
        ),
        (
            BackendSdk::Python,
            "reproit_sdk-1.0.0-py3-none-any.whl",
            "ManagedProjectToken",
            "run_operation",
        ),
        (
            BackendSdk::Rust,
            "reproit-sdk-rust-1.0.0.crate",
            "ManagedProjectToken::new",
            "OfficialManagedRustOperation",
        ),
    ];
    for (sdk, package, token_api, operation_api) in cases {
        let install = sdk_install_lines(sdk).join("\n");
        assert!(install.contains(package));
        assert!(!install.contains("crates.io"));
        assert!(!install.contains("npmjs"));
        assert!(!install.contains("pypi"));
        assert!(sdk_token_setup(sdk).contains(token_api));
        assert!(sdk_operation_setup(sdk).contains(operation_api));
    }
    assert_eq!(MANAGED_PROJECT_TOKEN_ENV, "REPROIT_MANAGED_PROJECT_TOKEN");
    let rust_setup = format!(
        "{}\n{}",
        sdk_token_setup(BackendSdk::Rust),
        sdk_operation_setup(BackendSdk::Rust)
    );
    assert!(rust_setup.contains("OfficialManagedProject::from_build"));
    for hidden_input in [
        "endpoint",
        "certificate",
        "signer",
        "key path",
        "framework",
        "container",
    ] {
        assert!(!rust_setup.contains(hidden_input));
    }
    assert!(!rust_setup.contains("Sdk::begin"));
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
