use reproit_backend::config::{BackendSdk, RunSpec};
use reproit_core::{Error, ErrorCode};

pub(crate) const AUTOMATIC_CAPTURE_CAPABILITY: &str = "capture.automatic-world.v1";
const RELEASED_SDK_VERSION: &str = "1.0.0";
const MAX_DECLARED_CAPABILITIES: usize = 16;
const MAX_CAPABILITY_BYTES: usize = 128;
const NODE_REGISTER_FLAG: &str = "--import";
const NODE_REGISTER_MODULE: &str = "@reproit/sdk/register";
const PYTHON_MODULE_FLAG: &str = "-m";
const PYTHON_REGISTER_MODULE: &str = "reproit_sdk.register";
const PYTHON_TARGET_SEPARATOR: &str = "--";
const GO_REBUILD_FLAG: &str = "-a";
const GO_TOOLEXEC_FLAG: &str = "-toolexec=reproit";

#[derive(Clone, Copy, Debug)]
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

pub(crate) fn normalize_startup_run(sdk: BackendSdk, run: RunSpec) -> Result<RunSpec, Error> {
    match sdk {
        BackendSdk::Go => normalize_go_run(run),
        BackendSdk::Nodejs => normalize_node_run(run),
        BackendSdk::Python => normalize_python_run(run),
        BackendSdk::Dotnet | BackendSdk::Rust => Ok(run),
    }
}

fn normalize_go_run(mut run: RunSpec) -> Result<RunSpec, Error> {
    if !is_direct_go_program(&run.program)
        || !matches!(run.arguments.first(), Some(command) if command == "run")
    {
        return Err(unsupported_go_run());
    }
    let exact_prefix = matches!(
        run.arguments.as_slice(),
        [command, rebuild, toolexec, ..]
            if command == "run"
                && rebuild == GO_REBUILD_FLAG
                && toolexec == GO_TOOLEXEC_FLAG
    );
    if exact_prefix {
        return Ok(run);
    }
    if run.arguments.iter().any(|argument| {
        argument == GO_REBUILD_FLAG || argument == GO_TOOLEXEC_FLAG || argument == "-toolexec"
    }) {
        return Err(unsupported_go_run());
    }
    let mut arguments = Vec::with_capacity(run.arguments.len().saturating_add(2));
    arguments.push("run".to_owned());
    arguments.push(GO_REBUILD_FLAG.to_owned());
    arguments.push(GO_TOOLEXEC_FLAG.to_owned());
    arguments.extend(run.arguments.drain(1..));
    run.arguments = arguments;
    Ok(run)
}

fn normalize_node_run(mut run: RunSpec) -> Result<RunSpec, Error> {
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

fn normalize_python_run(mut run: RunSpec) -> Result<RunSpec, Error> {
    if !is_direct_python_program(&run.program) {
        return Err(unsupported_python_run());
    }
    let exact_prefix = matches!(
        run.arguments.as_slice(),
        [module_flag, register_module, separator, ..]
            if module_flag == PYTHON_MODULE_FLAG
                && register_module == PYTHON_REGISTER_MODULE
                && separator == PYTHON_TARGET_SEPARATOR
    );
    let target = if exact_prefix {
        &run.arguments[3..]
    } else {
        if matches!(
            run.arguments.as_slice(),
            [module_flag, register_module, ..]
                if module_flag == PYTHON_MODULE_FLAG
                    && register_module == PYTHON_REGISTER_MODULE
        ) {
            return Err(unsupported_python_run());
        }
        run.arguments.as_slice()
    };
    if !valid_python_target(target) {
        return Err(unsupported_python_run());
    }
    if exact_prefix {
        return Ok(run);
    }
    let mut arguments = Vec::with_capacity(run.arguments.len().saturating_add(3));
    arguments.push(PYTHON_MODULE_FLAG.to_owned());
    arguments.push(PYTHON_REGISTER_MODULE.to_owned());
    arguments.push(PYTHON_TARGET_SEPARATOR.to_owned());
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

fn is_direct_python_program(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or_default();
    let lowercase = name.to_ascii_lowercase();
    let executable = lowercase.strip_suffix(".exe").unwrap_or(&lowercase);
    executable == "python"
        || executable == "python3"
        || executable.strip_prefix("python3.").is_some_and(|version| {
            !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn is_direct_go_program(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or_default();
    matches!(name.to_ascii_lowercase().as_str(), "go" | "go.exe")
}

fn valid_python_target(arguments: &[String]) -> bool {
    let Some((target, remaining)) = arguments.split_first() else {
        return false;
    };
    if target == PYTHON_MODULE_FLAG {
        let Some((module, _)) = remaining.split_first() else {
            return false;
        };
        return valid_python_module(module);
    }
    !target.is_empty() && !target.starts_with('-') && !target.chars().any(char::is_control)
}

fn valid_python_module(module: &str) -> bool {
    !module.is_empty()
        && module.split('.').all(|component| {
            let mut characters = component.chars();
            characters.next().is_some_and(|first| {
                (first.is_ascii_alphabetic() || first == '_')
                    && characters
                        .all(|character| character.is_ascii_alphanumeric() || character == '_')
            })
        })
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

fn unsupported_python_run() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        concat!(
            "Use python or python3 as the Python run program. ",
            "Put one script or -m module after it.",
        ),
    )
}

fn unsupported_go_run() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        concat!(
            "Use go run as the Go run program. ",
            "Put the application package after run.",
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
        capabilities: match sdk {
            BackendSdk::Go | BackendSdk::Nodejs | BackendSdk::Python => {
                &[AUTOMATIC_CAPTURE_CAPABILITY]
            }
            BackendSdk::Dotnet | BackendSdk::Rust => &[],
        },
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

    const SUPPORTED_SDKS: [BackendSdk; 3] =
        [BackendSdk::Go, BackendSdk::Nodejs, BackendSdk::Python];

    #[test]
    fn every_released_sdk_declares_automatic_capture() {
        for sdk in SUPPORTED_SDKS {
            let declaration = released_sdk(sdk).unwrap();
            assert_eq!(declaration.version, RELEASED_SDK_VERSION);
            assert_eq!(declaration.capabilities, [AUTOMATIC_CAPTURE_CAPABILITY]);
            assert!(declaration.install_command.contains("1.0.0"));
        }
    }

    #[test]
    fn incomplete_sdk_releases_are_not_declared_supported() {
        for sdk in [BackendSdk::Dotnet, BackendSdk::Rust] {
            let error = released_sdk(sdk).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnsupportedCapabilitySet);
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
    fn python_script_run_gets_the_registration_wrapper() {
        let normalized = normalize_startup_run(
            BackendSdk::Python,
            run("python3", &["service.py", "--port", "8080"]),
        )
        .unwrap();
        assert_eq!(
            normalized.arguments,
            [
                "-m",
                "reproit_sdk.register",
                "--",
                "service.py",
                "--port",
                "8080",
            ]
        );
    }

    #[test]
    fn python_module_run_gets_the_registration_wrapper() {
        let normalized = normalize_startup_run(
            BackendSdk::Python,
            run("python", &["-m", "orders.worker", "--queue", "urgent"]),
        )
        .unwrap();
        assert_eq!(
            normalized.arguments,
            [
                "-m",
                "reproit_sdk.register",
                "--",
                "-m",
                "orders.worker",
                "--queue",
                "urgent",
            ]
        );
    }

    #[test]
    fn python_registration_wrapper_is_idempotent() {
        for target in [
            vec!["service.py"],
            vec!["-m", "orders.worker", "--queue", "urgent"],
        ] {
            let mut arguments = vec!["-m", "reproit_sdk.register", "--"];
            arguments.extend(target);
            let original = run("python3", &arguments);
            assert_eq!(
                normalize_startup_run(BackendSdk::Python, original.clone()).unwrap(),
                original
            );
        }
    }

    #[test]
    fn python_run_accepts_direct_unix_and_windows_paths() {
        for program in [
            "/usr/bin/python3",
            "/opt/python/bin/python3.13",
            r"C:\Python313\python.exe",
        ] {
            let normalized =
                normalize_startup_run(BackendSdk::Python, run(program, &["service.py"])).unwrap();
            assert_eq!(normalized.program, program);
            assert_eq!(
                normalized.arguments,
                ["-m", "reproit_sdk.register", "--", "service.py"]
            );
        }
    }

    #[test]
    fn python_run_rejects_wrappers_and_invalid_targets_with_one_action() {
        let cases = [
            ("uv", vec!["run", "service.py"]),
            ("poetry", vec!["run", "python", "service.py"]),
            ("py", vec!["service.py"]),
            ("python", vec![]),
            ("python", vec!["-c", "print('no')"]),
            ("python", vec!["-u", "service.py"]),
            ("python", vec!["-m"]),
            ("python", vec!["-m", ""]),
            ("python", vec!["-m", "orders/worker"]),
            ("python", vec!["-m", "orders..worker"]),
            ("python", vec!["-m", "reproit_sdk.register"]),
            ("python", vec!["-m", "reproit_sdk.register", "--"]),
        ];
        for (program, arguments) in cases {
            let error =
                normalize_startup_run(BackendSdk::Python, run(program, &arguments)).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConfigConflict);
            assert_eq!(
                error.message,
                concat!(
                    "Use python or python3 as the Python run program. ",
                    "Put one script or -m module after it.",
                )
            );
        }
    }

    #[test]
    fn go_run_gets_internal_build_instrumentation() {
        let normalized = normalize_startup_run(
            BackendSdk::Go,
            run("go", &["run", "./cmd/service", "--port", "8080"]),
        )
        .unwrap();
        assert_eq!(
            normalized.arguments,
            [
                "run",
                "-a",
                "-toolexec=reproit",
                "./cmd/service",
                "--port",
                "8080",
            ]
        );
    }

    #[test]
    fn go_run_instrumentation_is_idempotent() {
        let original = run(
            "/usr/local/bin/go",
            &["run", "-a", "-toolexec=reproit", "./cmd/service"],
        );
        assert_eq!(
            normalize_startup_run(BackendSdk::Go, original.clone()).unwrap(),
            original
        );
    }

    #[test]
    fn go_run_rejects_wrappers_and_ambiguous_flags() {
        for (program, arguments) in [
            ("make", vec!["run"]),
            ("go", vec!["build", "./cmd/service"]),
            ("go", vec!["run", "-toolexec", "other", "./cmd/service"]),
        ] {
            let error =
                normalize_startup_run(BackendSdk::Go, run(program, &arguments)).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConfigConflict);
            assert_eq!(
                error.message,
                concat!(
                    "Use go run as the Go run program. ",
                    "Put the application package after run.",
                )
            );
        }
    }

    #[test]
    fn other_sdk_run_arrays_do_not_change() {
        for sdk in [BackendSdk::Dotnet, BackendSdk::Rust] {
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
