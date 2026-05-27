//! Protocol constants and enums derived from the REPE spec.

/// Magic spec value to denote the REPE specification (0x1507)
pub const REPE_SPEC: u16 = 0x1507;

/// Current REPE version (1)
pub const REPE_VERSION: u8 = 1;

/// Fixed header size in bytes
pub const HEADER_SIZE: usize = 48;

/// REPE high-level error codes.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ErrorCode {
    Ok = 0,
    VersionMismatch = 1,
    InvalidHeader = 2,
    InvalidQuery = 3,
    InvalidBody = 4,
    ParseError = 5,
    MethodNotFound = 6,
    Timeout = 7,
    /// The server is temporarily unable to service the request and the
    /// client should retry. Distinct from an application error: it
    /// signals transient saturation, not a failed result. The built-in
    /// `WebSocketServer` returns this when an off-reader request is
    /// rejected because the per-connection `with_offreader_limit` cap is
    /// reached. Occupies the REPE-reserved `8..4095` range (between
    /// `Timeout` and `ApplicationErrorBase`).
    ResourceExhausted = 8,
    /// The server hit an unexpected internal failure while handling the
    /// request; the result is not a normal application-level outcome.
    /// The built-in `WebSocketServer` returns this for a caught
    /// off-reader handler panic. Also in the REPE-reserved `8..4095`
    /// range.
    InternalError = 9,
    /// Application-specific errors start at 4096
    ApplicationErrorBase = 4096,
}

impl From<ErrorCode> for u32 {
    fn from(v: ErrorCode) -> Self {
        v as u32
    }
}

impl core::convert::TryFrom<u32> for ErrorCode {
    type Error = u32;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        let res = match value {
            0 => ErrorCode::Ok,
            1 => ErrorCode::VersionMismatch,
            2 => ErrorCode::InvalidHeader,
            3 => ErrorCode::InvalidQuery,
            4 => ErrorCode::InvalidBody,
            5 => ErrorCode::ParseError,
            6 => ErrorCode::MethodNotFound,
            7 => ErrorCode::Timeout,
            8 => ErrorCode::ResourceExhausted,
            9 => ErrorCode::InternalError,
            4096 => ErrorCode::ApplicationErrorBase,
            _ => return Err(value),
        };
        Ok(res)
    }
}

/// Reserved Query formats (0..=4095 reserved for REPE)
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueryFormat {
    RawBinary = 0,
    JsonPointer = 1,
}

impl From<QueryFormat> for u16 {
    fn from(v: QueryFormat) -> Self {
        v as u16
    }
}

impl core::convert::TryFrom<u16> for QueryFormat {
    type Error = u16;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => QueryFormat::RawBinary,
            1 => QueryFormat::JsonPointer,
            other => return Err(other),
        })
    }
}

/// Reserved Body formats (0..=4095 reserved for REPE)
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BodyFormat {
    RawBinary = 0,
    Beve = 1, // BEVE binary body
    Json = 2,
    Utf8 = 3,
}

impl From<BodyFormat> for u16 {
    fn from(v: BodyFormat) -> Self {
        v as u16
    }
}

impl core::convert::TryFrom<u16> for BodyFormat {
    type Error = u16;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => BodyFormat::RawBinary,
            1 => BodyFormat::Beve,
            2 => BodyFormat::Json,
            3 => BodyFormat::Utf8,
            other => return Err(other),
        })
    }
}
