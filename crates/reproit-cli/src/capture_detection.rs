use reproit_core::{Error, ErrorCode};

pub(crate) fn require_automatic_capture() -> Result<(), Error> {
    Err(Error::new(
        ErrorCode::UnsupportedCapabilitySet,
        "Automatic World capture support is not installed.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_automatic_capture_stops_initialization() {
        let error = require_automatic_capture().unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedCapabilitySet);
        assert_eq!(
            error.message,
            "Automatic World capture support is not installed."
        );
    }
}
