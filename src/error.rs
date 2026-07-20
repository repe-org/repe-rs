use crate::constants::ErrorCode;
use std::fmt::{Display, Formatter};

/// A REPE protocol, transport, or codec failure.
///
/// Marked `#[non_exhaustive]`: match with a `_` arm. New variants are added as
/// the protocol and its transports grow, and requiring a major release for each
/// one would price a clearer error out of ever shipping.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RepeError {
    #[error("version mismatch: {0}")]
    VersionMismatch(u8),
    #[error("invalid spec magic: 0x{0:04x}")]
    InvalidSpec(u16),
    #[error("invalid header length: expected 48, got {0}")]
    InvalidHeaderLength(usize),
    #[error("header length mismatch: expected {expected}, got {got}")]
    LengthMismatch { expected: u64, got: u64 },
    #[error("buffer too small: need {need} bytes, have {have}")]
    BufferTooSmall { need: usize, have: usize },
    #[error("response id mismatch: expected {expected}, got {got}")]
    ResponseIdMismatch { expected: u64, got: u64 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("beve error: {0}")]
    Beve(#[from] beve::Error),
    #[error("unknown value for enum conversion: {0}")]
    UnknownEnumValue(u64),
    #[error("unexpected body format: expected {expected:?}, got format code {got}")]
    UnexpectedBodyFormat {
        expected: crate::constants::BodyFormat,
        got: u16,
    },
    #[error("server error {code}: {message}")]
    ServerError { code: ErrorCode, message: String },
    /// An outbound message exceeded the peer's assumed maximum frame size and
    /// was not sent.
    ///
    /// This is a *pre-send* check against a configured assumption, not an
    /// observation. WebSocket size limits are enforced by the **reader**, so a
    /// sender has no way to discover what the peer will accept: the local write
    /// succeeds, and the peer's reader closes the connection. Left unguarded,
    /// an oversized message costs the sender a dropped connection with no error
    /// at all, and a reconnecting client that re-requests the same payload
    /// loops forever. Substituting this error keeps the connection alive and
    /// tells the sender exactly what happened.
    ///
    /// `limit` is the configured assumption, not a value learned from the peer.
    #[error(
        "outbound message of {size} bytes exceeds the assumed peer frame limit of {limit} bytes"
    )]
    MessageTooLarge { size: usize, limit: usize },
}

impl RepeError {
    pub fn to_error_code(&self) -> ErrorCode {
        match self {
            RepeError::VersionMismatch(_) => ErrorCode::VersionMismatch,
            RepeError::InvalidSpec(_) => ErrorCode::InvalidHeader,
            RepeError::InvalidHeaderLength(_) => ErrorCode::InvalidHeader,
            RepeError::LengthMismatch { .. } => ErrorCode::InvalidHeader,
            RepeError::BufferTooSmall { .. } => ErrorCode::ParseError,
            RepeError::ResponseIdMismatch { .. } => ErrorCode::InvalidHeader,
            RepeError::Io(_) => ErrorCode::ParseError,
            RepeError::Json(_) => ErrorCode::ParseError,
            RepeError::Beve(_) => ErrorCode::ParseError,
            RepeError::UnknownEnumValue(_) => ErrorCode::ParseError,
            RepeError::UnexpectedBodyFormat { .. } => ErrorCode::InvalidBody,
            RepeError::ServerError { code, .. } => *code,
            // Not `ResourceExhausted`: that code tells the client to retry, and
            // retrying an oversized response reproduces it exactly. The handler
            // produced a result that cannot be delivered, which is the internal
            // condition `InternalError` describes.
            RepeError::MessageTooLarge { .. } => ErrorCode::InternalError,
        }
    }
}

impl Display for ErrorCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorCode::Ok => "OK",
            ErrorCode::VersionMismatch => "Version mismatch",
            ErrorCode::InvalidHeader => "Invalid header",
            ErrorCode::InvalidQuery => "Invalid query",
            ErrorCode::InvalidBody => "Invalid body",
            ErrorCode::ParseError => "Parse error",
            ErrorCode::MethodNotFound => "Method not found",
            ErrorCode::Timeout => "Timeout",
            ErrorCode::ResourceExhausted => "Resource exhausted",
            ErrorCode::InternalError => "Internal error",
            ErrorCode::ApplicationErrorBase => "Application error",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_error_code_matches_variants() {
        let cases = vec![
            (RepeError::VersionMismatch(2), ErrorCode::VersionMismatch),
            (RepeError::InvalidSpec(0x1234), ErrorCode::InvalidHeader),
            (RepeError::InvalidHeaderLength(10), ErrorCode::InvalidHeader),
            (
                RepeError::LengthMismatch {
                    expected: 5,
                    got: 3,
                },
                ErrorCode::InvalidHeader,
            ),
            (
                RepeError::BufferTooSmall { need: 10, have: 1 },
                ErrorCode::ParseError,
            ),
            (
                RepeError::ResponseIdMismatch {
                    expected: 1,
                    got: 2,
                },
                ErrorCode::InvalidHeader,
            ),
            (
                RepeError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "io")),
                ErrorCode::ParseError,
            ),
            (
                RepeError::Json(serde_json::Error::io(std::io::Error::other("json"))),
                ErrorCode::ParseError,
            ),
            (
                RepeError::Beve(beve::Error::msg("beve")),
                ErrorCode::ParseError,
            ),
            (RepeError::UnknownEnumValue(9), ErrorCode::ParseError),
            (
                RepeError::UnexpectedBodyFormat {
                    expected: crate::constants::BodyFormat::Beve,
                    got: 2,
                },
                ErrorCode::InvalidBody,
            ),
            (
                RepeError::ServerError {
                    code: ErrorCode::Timeout,
                    message: "timeout".into(),
                },
                ErrorCode::Timeout,
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(err.to_error_code(), expected);
        }
    }

    #[test]
    fn new_error_codes_round_trip_on_the_wire() {
        // Clients decode `header.ec` via `ErrorCode::try_from`; the new
        // codes must survive that round-trip or they would degrade to
        // `ParseError` on the receiving end.
        assert_eq!(u32::from(ErrorCode::ResourceExhausted), 8);
        assert_eq!(u32::from(ErrorCode::InternalError), 9);
        for code in [ErrorCode::ResourceExhausted, ErrorCode::InternalError] {
            assert_eq!(ErrorCode::try_from(u32::from(code)).unwrap(), code);
        }
    }
}
