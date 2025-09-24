use crate::constants::ErrorCode;
use std::fmt::{Display, Formatter};

#[derive(Debug, thiserror::Error)]
pub enum RepeError {
    #[error("version mismatch: {0}")]
    VersionMismatch(u8),
    #[error("invalid spec magic: 0x{0:04x}")]
    InvalidSpec(u16),
    #[error("invalid header length: expected 48, got {0}")]
    InvalidHeaderLength(usize),
    #[error("header reserved field must be zero")]
    ReservedNonZero,
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
    #[error("server error {code}: {message}")]
    ServerError { code: ErrorCode, message: String },
}

impl RepeError {
    pub fn to_error_code(&self) -> ErrorCode {
        match self {
            RepeError::VersionMismatch(_) => ErrorCode::VersionMismatch,
            RepeError::InvalidSpec(_) => ErrorCode::InvalidHeader,
            RepeError::InvalidHeaderLength(_) => ErrorCode::InvalidHeader,
            RepeError::ReservedNonZero => ErrorCode::InvalidHeader,
            RepeError::LengthMismatch { .. } => ErrorCode::InvalidHeader,
            RepeError::BufferTooSmall { .. } => ErrorCode::ParseError,
            RepeError::ResponseIdMismatch { .. } => ErrorCode::InvalidHeader,
            RepeError::Io(_) => ErrorCode::ParseError,
            RepeError::Json(_) => ErrorCode::ParseError,
            RepeError::Beve(_) => ErrorCode::ParseError,
            RepeError::UnknownEnumValue(_) => ErrorCode::ParseError,
            RepeError::ServerError { code, .. } => *code,
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
            (RepeError::ReservedNonZero, ErrorCode::InvalidHeader),
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
}
