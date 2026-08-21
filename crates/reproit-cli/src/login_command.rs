use std::{
    process::{Command, Stdio},
    time::Duration,
};

use reproit_cli::render::stdout_line;
use reproit_cli::{
    LoginAttempt, NativeCredentialStore, discover_oauth_metadata, exchange_authorization_code,
};
use reproit_core::{Error, ErrorCode};

const OFFICIAL_AUTHORITY: Option<&str> = option_env!("REPROIT_OFFICIAL_CLI_AUTHORITY");
const OFFICIAL_CLIENT_ID: Option<&str> = option_env!("REPROIT_OFFICIAL_CLI_CLIENT_ID");
const MAX_AUTHORITY_BYTES: usize = 2_048;
const MAX_CLIENT_ID_BYTES: usize = 512;

pub(crate) struct LoginConfiguration {
    pub(crate) authority: String,
    pub(crate) client_id: String,
}

pub(crate) async fn run() -> Result<(), Error> {
    let configuration = login_configuration()?;
    let metadata = discover_oauth_metadata(&configuration.authority).await?;
    let attempt = LoginAttempt::new(&metadata, &configuration.client_id)?;
    open_browser(attempt.authorization_url())?;
    let result = attempt.wait_for_callback(Duration::from_mins(2))?;
    let session = exchange_authorization_code(&metadata, &configuration.client_id, &result).await?;
    NativeCredentialStore::open()?.store(&session)?;
    stdout_line(format_args!("Logged in."))?;
    stdout_line(format_args!("Run reproit init."))?;
    Ok(())
}

fn login_configuration() -> Result<LoginConfiguration, Error> {
    let authority = read_override("REPROIT_AUTHORITY", MAX_AUTHORITY_BYTES)?;
    let client_id = read_override("REPROIT_CLI_CLIENT_ID", MAX_CLIENT_ID_BYTES)?;
    select_login_configuration(
        authority.as_deref(),
        client_id.as_deref(),
        OFFICIAL_AUTHORITY,
        OFFICIAL_CLIENT_ID,
    )
}

pub(crate) fn select_login_configuration(
    authority_override: Option<&str>,
    client_id_override: Option<&str>,
    official_authority: Option<&str>,
    official_client_id: Option<&str>,
) -> Result<LoginConfiguration, Error> {
    let (authority, client_id) = match (authority_override, client_id_override) {
        (None, None) => (
            official_authority.ok_or_else(missing_official_metadata)?,
            official_client_id.ok_or_else(missing_official_metadata)?,
        ),
        (Some(authority), Some(client_id)) => (authority, client_id),
        _ => return Err(invalid_configuration()),
    };
    if !valid_authority(authority) || !valid_client_id(client_id) {
        return Err(invalid_configuration());
    }
    Ok(LoginConfiguration {
        authority: authority.to_owned(),
        client_id: client_id.to_owned(),
    })
}

fn read_override(name: &str, max_bytes: usize) -> Result<Option<String>, Error> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() && value.len() <= max_bytes => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => Err(invalid_configuration()),
        Err(std::env::VarError::NotPresent) => Ok(None),
    }
}

fn valid_authority(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_AUTHORITY_BYTES && !value.chars().any(char::is_control)
}

fn valid_client_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CLIENT_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn missing_official_metadata() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        "The released CLI does not contain its public OAuth metadata.",
    )
}

fn invalid_configuration() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        "The CLI login configuration is invalid.",
    )
}

fn open_browser(url: &str) -> Result<(), Error> {
    #[cfg(target_os = "macos")]
    let status = browser_command("open", &[url]);
    #[cfg(target_os = "linux")]
    let status = browser_command("xdg-open", &[url]);
    #[cfg(target_os = "windows")]
    let status = browser_command("rundll32", &["url.dll,FileProtocolHandler", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let status = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "browser launch is unsupported",
    ));
    if status.is_ok_and(|status| status.success()) {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::ServiceUnavailable,
            "Repro It could not open the authentication page.",
        ))
    }
}

fn browser_command(program: &str, arguments: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    Command::new(program)
        .args(arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}
