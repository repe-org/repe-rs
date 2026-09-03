use crate::constants::ErrorCode;

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
    /// A header advertised query and body lengths whose framed total cannot be
    /// represented, so no buffer could ever hold the message it describes.
    ///
    /// Both lengths are attacker-controlled `u64`s read straight off the wire.
    /// Summing them with the 48-byte header unchecked wraps, and a wrapped total
    /// can be made to agree with the header's own `length` field, which is how a
    /// 48-byte frame reaches slicing code with bounds that run backwards. This
    /// rejects the frame at the header instead, so every later `as usize` in the
    /// crate is operating on a total that fits both `u64` and this target's
    /// address space.
    #[error(
        "frame lengths are not representable: 48-byte header + query {query_length} + body {body_length}"
    )]
    FrameLengthOverflow { query_length: u64, body_length: u64 },
    #[error("response id mismatch: expected {expected}, got {got}")]
    ResponseIdMismatch { expected: u64, got: u64 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON body did not parse.
    ///
    /// structio has one `Error` for both formats and deliberately does not
    /// record which one produced it — the reasoning being that the caller knows,
    /// because it chose the parser. repe does: every decode site here has
    /// already read the frame header and so has the `BodyFormat` in hand. These
    /// two variants are that knowledge written down, which is why neither
    /// carries a `#[from]`: an impl could not tell them apart.
    #[error("json error: {0}")]
    Json(#[source] structio::Error),
    /// A BEVE body did not parse.
    #[error("beve error: {0}")]
    Beve(#[source] structio::Error),
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
    /// A parse failure attributed to the format the frame header declared.
    ///
    /// The one place the `Json` / `Beve` split is made, so a decode site names
    /// its format once rather than choosing a variant by hand.
    pub fn decode(format: crate::constants::BodyFormat, source: structio::Error) -> Self {
        match format {
            crate::constants::BodyFormat::Beve => RepeError::Beve(source),
            _ => RepeError::Json(source),
        }
    }

    /// [`decode`](Self::decode) for a streaming read, whose failure may be I/O
    /// rather than content.
    ///
    /// An `io::Error` stays an `io::Error` all the way up rather than being
    /// flattened into a parse error. There is deliberately no
    /// `From<structio::StreamError>` beside this: `StreamError` is
    /// format-independent — `json::Documents` and `json::Feed` raise the same
    /// `Parse` variant that BEVE does — so a `From` impl would have to guess
    /// which format failed, which is exactly what these two variants exist to
    /// avoid.
    pub fn decode_stream(format: crate::constants::BodyFormat, err: structio::StreamError) -> Self {
        match err {
            structio::StreamError::Io(err) => RepeError::Io(err),
            structio::StreamError::Parse(err) => RepeError::decode(format, err),
            // `StreamError` is non-exhaustive. A variant added later is content
            // rather than I/O until it says otherwise, so it is attributed to
            // the format like any other parse failure.
            _ => RepeError::decode(
                format,
                structio::Error::new(structio::ErrorCode::InvalidUtf8, 0),
            ),
        }
    }

    pub fn to_error_code(&self) -> ErrorCode {
        match self {
            RepeError::VersionMismatch(_) => ErrorCode::VersionMismatch,
            RepeError::InvalidSpec(_) => ErrorCode::InvalidHeader,
            RepeError::InvalidHeaderLength(_) => ErrorCode::InvalidHeader,
            RepeError::LengthMismatch { .. } => ErrorCode::InvalidHeader,
            RepeError::BufferTooSmall { .. } => ErrorCode::ParseError,
            RepeError::FrameLengthOverflow { .. } => ErrorCode::InvalidHeader,
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

/// Returned by a client's `subscribe_notifies` when a live subscription
/// already exists.
///
/// "Live" means the prior receiver has not been dropped. To replace a live
/// subscription, call `unsubscribe_notifies` first. A subscription whose
/// receiver has already been dropped does not block resubscription:
/// `subscribe_notifies` silently replaces the stale slot and returns the new
/// receiver.
///
/// Shared by `WebSocketClient` and `WasmClient` so the one-subscriber contract
/// reads the same on both transports. Lives here rather than in either client
/// because both modules are feature- and target-gated, and neither can see the
/// other.
#[cfg(any(feature = "websocket", feature = "websocket-wasm"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadySubscribed;

#[cfg(any(feature = "websocket", feature = "websocket-wasm"))]
impl std::fmt::Display for AlreadySubscribed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a notify subscription is already active on this client")
    }
}

#[cfg(any(feature = "websocket", feature = "websocket-wasm"))]
impl std::error::Error for AlreadySubscribed {}

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
                RepeError::Json(structio::Error::new(structio::ErrorCode::ExpectedBrace, 0)),
                ErrorCode::ParseError,
            ),
            (
                RepeError::Beve(structio::Error::new(structio::ErrorCode::ExpectedObject, 0)),
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
