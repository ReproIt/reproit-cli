use reproit_backend::config::{BackendSdk, RunSpec};
use reproit_core::{Error, ErrorCode};

pub(crate) const AUTOMATIC_CAPTURE_CAPABILITY: &str = "capture.automatic-world.v1";
const RELEASED_SDK_VERSION: &str = "1.0.0";
const MAX_DECLARED_CAPABILITIES: usize = 16;
const MAX_CAPABILITY_BYTES: usize = 128;
const NODE_REGISTER_FLAG: &str = "--import";
const NODE_REGISTER_MODULE: &str = "@reproit/sdk/register";

#[derive(Clone, Copy)]
pub(crate) struct ReleasedSdkDeclaration {
    pub(crate) install_command: &'static str,
    sdk: BackendSdk,
    version: &'static str,
    capabilities: &'static [&'static str],
}

pub(crate) fn released_sdk(sdk: BackendSdk) -> Result<ReleasedSdkDeclaration, Error> {
    let declaration = declaration(sdk);
    validate_declaration(sdk, &declaration)?;
    Ok(declaration)
}

pub(crate) fn normalize_startup_run(sdk: BackendSdk, mut run: RunSpec) -> Result<RunSpec, Error> {
    if sdk != BackendSdk::Nodejs {
        return Ok(run);
    }
    if !is_direct_node_program(&run.program) {
        return Err(unsupported_node_run_program());
    }
    if matches!(
        run.arguments.as_slice(),
        [flag, module, ..] if flag == NODE_REGISTER_FLAG && module == NODE_REGISTER_MODULE
    ) {
        return Ok(run);
    }
    let mut arguments = Vec::with_capacity(run.arguments.len().saturating_add(2));
    arguments.push(NODE_REGISTER_FLAG.to_owned());
    arguments.push(NODE_REGISTER_MODULE.to_owned());
    arguments.append(&mut run.arguments);
    run.arguments = arguments;
    Ok(run)
}

fn is_direct_node_program(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or_default();
    matches!(
        name.to_ascii_lowercase().as_str(),
        "node" | "node.exe" | "nodejs" | "nodejs.exe"
    )
}

fn unsupported_node_run_program() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        concat!(
            "Use node or nodejs as the Node.js run program. ",
            "Put the application script after the program.",
        ),
    )
}

fn declaration(sdk: BackendSdk) -> ReleasedSdkDeclaration {
    let install_command = match sdk {
        BackendSdk::Dotnet => "dotnet add package ReproIt.Sdk --version 1.0.0",
        BackendSdk::Go => "go get reproit.dev/sdk-go@v1.0.0",
        BackendSdk::Nodejs => "npm install @reproit/sdk@1.0.0",
        BackendSdk::Python => "python -m pip install reproit-sdk==1.0.0",
        BackendSdk::Rust => "cargo add reproit-sdk-rust@1.0.0",
    };
    ReleasedSdkDeclaration {
        install_command,
        sdk,
        version: RELEASED_SDK_VERSION,
        capabilities: &[AUTOMATIC_CAPTURE_CAPABILITY],
    }
}

fn validate_declaration(
    selected: BackendSdk,
    declaration: &ReleasedSdkDeclaration,
) -> Result<(), Error> {
    let capabilities = declaration.capabilities;
    if declaration.sdk != selected
        || declaration.version != RELEASED_SDK_VERSION
        || capabilities.is_empty()
        || capabilities.len() > MAX_DECLARED_CAPABILITIES
        || capabilities.iter().any(|capability| {
            capability.is_empty()
                || capability.len() > MAX_CAPABILITY_BYTES
                || !capability.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-')
                })
        })
        || !capabilities.windows(2).all(|pair| pair[0] < pair[1])
        || !capabilities.contains(&AUTOMATIC_CAPTURE_CAPABILITY)
    {
        return Err(unsupported_capture());
    }
    Ok(())
}

fn unsupported_capture() -> Error {
    Error::new(
        ErrorCode::UnsupportedCapabilitySet,
        "The selected SDK does not support automatic World capture.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDKS: [BackendSdk; 5] = [
        BackendSdk::Dotnet,
        BackendSdk::Go,
        BackendSdk::Nodejs,
        BackendSdk::Python,
        BackendSdk::Rust,
    ];

    #[test]
    fn every_released_sdk_declares_automatic_capture() {
        for sdk in SDKS {
            let declaration = released_sdk(sdk).unwrap();
            assert_eq!(declaration.version, RELEASED_SDK_VERSION);
            assert_eq!(declaration.capabilities, [AUTOMATIC_CAPTURE_CAPABILITY]);
            assert!(declaration.install_command.contains("1.0.0"));
        }
    }

    #[test]
    fn a_release_without_automatic_capture_is_rejected() {
        let declaration = ReleasedSdkDeclaration {
            install_command: "unused",
            sdk: BackendSdk::Rust,
            version: RELEASED_SDK_VERSION,
            capabilities: &["runtime.rust-native"],
        };
        let error = validate_declaration(BackendSdk::Rust, &declaration).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedCapabilitySet);
        assert_eq!(
            error.message,
            "The selected SDK does not support automatic World capture."
        );
    }

    #[test]
    fn a_declaration_for_another_sdk_is_rejected() {
        let declaration = declaration(BackendSdk::Python);
        assert!(validate_declaration(BackendSdk::Nodejs, &declaration).is_err());
    }

    #[test]
    fn node_run_gets_the_registration_prefix() {
        let normalized = normalize_startup_run(
            BackendSdk::Nodejs,
            run("node", &["service.mjs", "--port", "8080"]),
        )
        .unwrap();
        assert_eq!(
            normalized.arguments,
            [
                "--import",
                "@reproit/sdk/register",
                "service.mjs",
                "--port",
                "8080",
            ]
        );
    }

    #[test]
    fn node_run_registration_prefix_is_idempotent() {
        let original = run(
            "nodejs",
            &["--import", "@reproit/sdk/register", "service.mjs"],
        );
        assert_eq!(
            normalize_startup_run(BackendSdk::Nodejs, original.clone()).unwrap(),
            original
        );
    }

    #[test]
    fn node_run_accepts_direct_executable_paths() {
        for program in ["/usr/local/bin/node", "./tools/nodejs"] {
            let normalized =
                normalize_startup_run(BackendSdk::Nodejs, run(program, &["service.mjs"])).unwrap();
            assert_eq!(normalized.program, program);
            assert_eq!(
                normalized.arguments,
                ["--import", "@reproit/sdk/register", "service.mjs"]
            );
        }
    }

    #[test]
    fn node_run_accepts_a_windows_node_executable_path() {
        let program = r"C:\Program Files\nodejs\node.exe";
        let normalized =
            normalize_startup_run(BackendSdk::Nodejs, run(program, &["service.mjs"])).unwrap();
        assert_eq!(normalized.program, program);
        assert_eq!(
            normalized.arguments,
            ["--import", "@reproit/sdk/register", "service.mjs"]
        );
    }

    #[test]
    fn node_run_rejects_ambiguous_wrappers_with_one_action() {
        for program in ["npm", "yarn", "sh", "cmd.exe"] {
            let error =
                normalize_startup_run(BackendSdk::Nodejs, run(program, &["start"])).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConfigConflict);
            assert_eq!(
                error.message,
                concat!(
                    "Use node or nodejs as the Node.js run program. ",
                    "Put the application script after the program.",
                )
            );
        }
    }

    #[test]
    fn other_sdk_run_arrays_do_not_change() {
        for sdk in [
            BackendSdk::Dotnet,
            BackendSdk::Go,
            BackendSdk::Python,
            BackendSdk::Rust,
        ] {
            let original = run("wrapper", &["service", "--flag"]);
            assert_eq!(
                normalize_startup_run(sdk, original.clone()).unwrap(),
                original
            );
        }
    }

    fn run(program: &str, arguments: &[&str]) -> RunSpec {
        RunSpec {
            arguments: arguments.iter().map(|value| (*value).to_owned()).collect(),
            program: program.to_owned(),
            working_directory: ".".to_owned(),
        }
    }
}
