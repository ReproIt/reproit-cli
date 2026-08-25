use reproit_backend::config::BackendSdk;
use reproit_core::{Error, ErrorCode};

pub(crate) const AUTOMATIC_CAPTURE_CAPABILITY: &str = "capture.automatic-world.v1";
const RELEASED_SDK_VERSION: &str = "1.0.0";
const MAX_DECLARED_CAPABILITIES: usize = 16;
const MAX_CAPABILITY_BYTES: usize = 128;

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
}
