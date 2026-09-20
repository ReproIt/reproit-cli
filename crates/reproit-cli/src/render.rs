use std::{fmt, process::ExitCode};

use clap::error::{ContextKind, ContextValue, ErrorKind};

use reproit_core::{Error, ErrorCode};

use crate::authored_repro::AuthoredObservedResult;

const MAX_LINE_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy)]
pub enum PublicErrorContext {
    Check,
    Cloud,
    General,
    Init,
    Login,
    Release,
    Source,
}

pub fn stdout_line(arguments: fmt::Arguments<'_>) -> Result<(), Error> {
    write_line(std::io::stdout().lock(), arguments)
}

pub fn stderr_line(arguments: fmt::Arguments<'_>) {
    let _ = write_line(std::io::stderr().lock(), arguments);
}

pub fn parse_cli<Cli: clap::Parser>(binary_name: &str) -> Result<Cli, ExitCode> {
    match Cli::try_parse() {
        Ok(cli) => Ok(cli),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            Err(if error.print().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Err(error) if error.kind() == ErrorKind::InvalidSubcommand => {
            stderr_line(format_args!(
                "error: unrecognized command '{}'",
                unrecognized_command_name(&error)
            ));
            stderr_line(format_args!("Run '{binary_name} --help' for usage."));
            Err(ExitCode::from(2))
        }
        Err(_) => {
            stderr_line(format_args!("error: invalid command usage"));
            stderr_line(format_args!("Run '{binary_name} --help' for usage."));
            Err(ExitCode::from(2))
        }
    }
}

pub fn render_observed_results(results: &[AuthoredObservedResult]) -> Result<(), Error> {
    for observed in results {
        stdout_line(format_args!(
            "Observed {}: {} to {} {}. Median: {}. Expected: {} to {}.",
            observed.criterion_id,
            observed.observed_minimum,
            observed.observed_maximum,
            observed.unit,
            observed.observed_median,
            observed.expected_minimum,
            observed.expected_maximum,
        ))?;
    }
    Ok(())
}

pub fn render_error(context: PublicErrorContext, error: &Error, details: bool) {
    let (problem, action) = error_message(context, error);
    stderr_line(format_args!("{problem}"));
    if details && action == "Run again with --details." {
        stderr_line(format_args!(
            "Use the error code below when you report this problem."
        ));
    } else if details && action.starts_with("Run with --details") {
        stderr_line(format_args!("Ask an administrator to review the limit."));
    } else {
        stderr_line(format_args!("{action}"));
    }
    if details {
        stderr_line(format_args!("Code: {}", error.code.as_str()));
        stderr_line(format_args!(
            "Retryable: {}",
            if error.retryable { "yes" } else { "no" }
        ));
    }
}

pub fn structured_error(context: PublicErrorContext, error: &Error) -> Error {
    let (problem, action) = error_message(context, error);
    Error {
        code: error.code,
        message: format!("{problem} {action}"),
        retryable: error.retryable,
    }
}

fn error_message(context: PublicErrorContext, error: &Error) -> (&'static str, &'static str) {
    // Only fixed local messages can select these diagnostics. Never print an internal error.
    match (error.code, error.message.as_str()) {
        (ErrorCode::ConfigConflict, crate::PROJECT_MISSING) => (
            crate::PROJECT_MISSING,
            "Run reproit init from your application repository root.",
        ),
        (ErrorCode::SchemaInvalid, crate::PROJECT_INVALID) => (
            crate::PROJECT_INVALID,
            "Restore a valid project file, then run the command again.",
        ),
        (ErrorCode::EvaluationError, crate::PROJECT_UNREADABLE) => (
            crate::PROJECT_UNREADABLE,
            "Check that .reproit is a directory and project.toml is readable.",
        ),
        _ => public_error(context, error.code),
    }
}

pub const fn public_error(
    context: PublicErrorContext,
    code: ErrorCode,
) -> (&'static str, &'static str) {
    match code {
        ErrorCode::ConfigConflict if matches!(context, PublicErrorContext::Login) => (
            "The CLI login configuration is missing or invalid.",
            concat!(
                "Use an official release, or set both REPROIT_AUTHORITY and ",
                "REPROIT_CLI_CLIENT_ID for your test service."
            ),
        ),
        ErrorCode::AuthenticationRequired => ("Login is required.", "Run reproit login."),
        ErrorCode::AssigneeNotAuthorized
        | ErrorCode::AuthorizationDenied
        | ErrorCode::CrossTenantScope
        | ErrorCode::Forbidden => (
            "You do not have access to this action.",
            "Ask an organization administrator for access.",
        ),
        ErrorCode::SourceAccessDenied
        | ErrorCode::SourceCheckoutFailed
        | ErrorCode::SourceRevisionMissing => (
            "Repro It could not get the required source.",
            "Check your Git access, then try again.",
        ),
        ErrorCode::SourceDependencyMissing => (
            "Repro It could not prepare the source dependencies.",
            "Check the required toolchain, lockfile, and dependency access, then try again.",
        ),
        ErrorCode::UnsupportedCapabilitySet if matches!(context, PublicErrorContext::Init) => (
            "The application did not load complete automatic World capture.",
            concat!(
                "Install a supported SDK and use its direct application command, ",
                "then run reproit init again.",
            ),
        ),
        ErrorCode::UnsupportedCapabilitySet => (
            "No compatible replay host is available.",
            "Use a compatible replay host.",
        ),
        ErrorCode::KeepDestinationUnavailable => (
            "Repro It could not read the kept Repro.",
            "Check your connection, then try again.",
        ),
        ErrorCode::KeyProviderUnavailable | ErrorCode::KeyUnwrapFailed => (
            "Repro It could not unlock the kept Repro.",
            "Check your Repro It key access, then try again.",
        ),
        ErrorCode::ObjectDigestMismatch if matches!(context, PublicErrorContext::Release) => (
            "The evidence bundle failed verification.",
            "Restore the evidence bundle from a verified copy.",
        ),
        ErrorCode::ObjectDigestMismatch | ErrorCode::DecryptionAuthentication => (
            "The stored Repro failed verification.",
            "Restore it from a verified copy.",
        ),
        ErrorCode::RuntimeQuota | ErrorCode::RateLimited | ErrorCode::UploadLimitExceeded => (
            "Repro It stopped because this Repro exceeded a safety limit.",
            "Run with --details and ask an administrator to review the limit.",
        ),
        ErrorCode::TriageConflict => (
            "This Repro changed while you were editing it.",
            "Run reproit list, then try again.",
        ),
        ErrorCode::ServiceUnavailable if matches!(context, PublicErrorContext::Login) => (
            "Repro It could not complete login.",
            "Check your connection, then run reproit login again.",
        ),
        ErrorCode::ServiceUnavailable if matches!(context, PublicErrorContext::Cloud) => (
            "Repro It could not reach Cloud.",
            "Check your connection, then try again.",
        ),
        ErrorCode::ServiceUnavailable => (
            "Repro It could not reach Cloud.",
            "Check your connection, then try again.",
        ),
        ErrorCode::DifferentFailure if matches!(context, PublicErrorContext::Check) => (
            "One or more stored bugs reproduced.",
            "Review the REGRESSION results above.",
        ),
        _ => (
            "Repro It could not evaluate this Repro.",
            "Run again with --details.",
        ),
    }
}

fn write_line(mut writer: impl std::io::Write, arguments: fmt::Arguments<'_>) -> Result<(), Error> {
    let value = arguments.to_string();
    if value.len() > MAX_LINE_BYTES
        || value
            .chars()
            .any(|character| character.is_control() && character != '\t')
    {
        return Err(output_invalid());
    }
    writer
        .write_all(value.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .map_err(|_| output_invalid())
}

fn output_invalid() -> Error {
    Error::new(
        ErrorCode::EvaluationError,
        "Repro It could not write bounded command output.",
    )
}

// The command name is untrusted. Keep only bounded printable text before output.
fn unrecognized_command_name(error: &clap::Error) -> String {
    let name = error
        .get(ContextKind::InvalidSubcommand)
        .and_then(|value| match value {
            ContextValue::String(name) => Some(name.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let sanitized: String = name
        .chars()
        .filter(|character| character.is_ascii_graphic() && *character != '\'')
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_rejects_control_text_and_one_byte_over() {
        assert!(write_line(Vec::new(), format_args!("safe\tvalue")).is_ok());
        assert!(write_line(Vec::new(), format_args!("unsafe\nvalue")).is_err());
        let over = "a".repeat(MAX_LINE_BYTES + 1);
        assert!(write_line(Vec::new(), format_args!("{over}")).is_err());
    }

    #[test]
    fn structured_errors_replace_internal_messages() {
        let internal = Error::new(
            ErrorCode::EvaluationError,
            "The executor exposed a private route and credential.",
        );
        let public = structured_error(PublicErrorContext::General, &internal);
        assert_eq!(public.code, ErrorCode::EvaluationError);
        assert_eq!(
            public.message,
            "Repro It could not evaluate this Repro. Run again with --details."
        );
        for forbidden in ["executor", "private route", "credential"] {
            assert!(!public.message.contains(forbidden));
        }
    }

    #[test]
    fn initialization_reports_an_unsupported_sdk_declaration() {
        assert_eq!(
            public_error(
                PublicErrorContext::Init,
                ErrorCode::UnsupportedCapabilitySet,
            ),
            (
                "The application did not load complete automatic World capture.",
                concat!(
                    "Install a supported SDK and use its direct application command, ",
                    "then run reproit init again."
                )
            )
        );
    }
}
