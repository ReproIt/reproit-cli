use std::fmt;

use reproit_core::{Error, ErrorCode};

const MAX_LINE_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy)]
pub enum PublicErrorContext {
    Check,
    Cloud,
    General,
    Init,
    Login,
    Source,
}

pub fn stdout_line(arguments: fmt::Arguments<'_>) -> Result<(), Error> {
    write_line(std::io::stdout().lock(), arguments)
}

pub fn stderr_line(arguments: fmt::Arguments<'_>) {
    let _ = write_line(std::io::stderr().lock(), arguments);
}

pub fn render_error(context: PublicErrorContext, error: &Error, details: bool) {
    let (problem, action) = public_error(context, error.code);
    stderr_line(format_args!("{problem}"));
    stderr_line(format_args!("{action}"));
    if details {
        stderr_line(format_args!("Code: {}", error.code.as_str()));
        stderr_line(format_args!(
            "Retryable: {}",
            if error.retryable { "yes" } else { "no" }
        ));
    }
}

pub fn structured_error(context: PublicErrorContext, error: &Error) -> Error {
    let (problem, action) = public_error(context, error.code);
    Error {
        code: error.code,
        message: format!("{problem} {action}"),
        retryable: error.retryable,
    }
}

pub const fn public_error(
    context: PublicErrorContext,
    code: ErrorCode,
) -> (&'static str, &'static str) {
    match code {
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
        | ErrorCode::SourceDependencyMissing
        | ErrorCode::SourceRevisionMissing => (
            "Repro It could not get the required source.",
            "Check your Git access, then try again.",
        ),
        ErrorCode::UnsupportedCapabilitySet if matches!(context, PublicErrorContext::Init) => (
            "Automatic World capture support is not installed.",
            "Install a released SDK with complete automatic capture, then run reproit init again.",
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
    fn initialization_reports_the_automatic_capture_blocker() {
        assert_eq!(
            public_error(
                PublicErrorContext::Init,
                ErrorCode::UnsupportedCapabilitySet,
            ),
            (
                "Automatic World capture support is not installed.",
                concat!(
                    "Install a released SDK with complete automatic capture, ",
                    "then run reproit init again."
                )
            )
        );
    }
}
