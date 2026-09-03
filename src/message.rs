use crate::constants::{BodyFormat, ErrorCode, HEADER_SIZE, QueryFormat};
use crate::error::RepeError;
use crate::header::Header;
use std::borrow::Cow;
use std::io::{self, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub query: Vec<u8>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn new(header: Header, query: Vec<u8>, body: Vec<u8>) -> Result<Self, RepeError> {
        if header.query_length != query.len() as u64 || header.body_length != body.len() as u64 {
            return Err(RepeError::LengthMismatch {
                // Saturating: these are the caller's own advertised lengths, and
                // this arm is already reporting that they are wrong. A `u64` sum
                // that wraps here would turn a bad-input report into a panic in
                // debug builds, for no gain — the number only has to be large
                // enough to show the mismatch.
                expected: (HEADER_SIZE as u64)
                    .saturating_add(header.query_length)
                    .saturating_add(header.body_length),
                got: HEADER_SIZE as u64 + query.len() as u64 + body.len() as u64,
            });
        }
        Ok(Self {
            header,
            query,
            body,
        })
    }

    pub fn builder() -> MessageBuilder {
        MessageBuilder::default()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + self.query.len() + self.body.len());
        out.extend_from_slice(&self.header.encode());
        if !self.query.is_empty() {
            out.extend_from_slice(&self.query);
        }
        if !self.body.is_empty() {
            out.extend_from_slice(&self.body);
        }
        out
    }

    /// Wire size of the serialized message: `HEADER_SIZE + query.len() + body.len()`.
    /// O(1); does not serialize.
    pub fn serialized_len(&self) -> usize {
        HEADER_SIZE + self.query.len() + self.body.len()
    }

    /// Write the message to `w` without allocating an intermediate frame buffer.
    ///
    /// Equivalent to `w.write_all(&self.to_vec())` byte-for-byte, but emits the
    /// header, query, and body in three writes instead of building a single
    /// owned `Vec<u8>`. Useful when the body is large (e.g. multi-MiB chunks).
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.header.encode())?;
        if !self.query.is_empty() {
            w.write_all(&self.query)?;
        }
        if !self.body.is_empty() {
            w.write_all(&self.body)?;
        }
        Ok(())
    }

    /// Consume the message and produce its wire-frame bytes.
    ///
    /// Prefer this over [`to_vec`](Self::to_vec) on outbound paths where the
    /// `Message` is being shipped to a sink that takes an owned `Vec<u8>` (e.g.
    /// the WebSocket writer): `to_vec` always allocates a fresh frame buffer
    /// and copies the body into it, whereas `into_wire_bytes` reuses the body
    /// allocation when it already has the spare capacity to host the header
    /// and query prefix.
    ///
    /// Fast path (zero new allocations, one body memcpy to shift to the back):
    /// triggers when `body.capacity() >= HEADER_SIZE + query.len() +
    /// body.len()`. Callers that care about streaming throughput can guarantee
    /// the fast path by constructing the body via
    /// `Vec::with_capacity(body_len + HEADER_SIZE + query.len())` and then
    /// passing it to `MessageBuilder::body_bytes`.
    ///
    /// Slow path: when the body has no spare capacity for the prefix, a fresh
    /// `Vec<u8>` is allocated. The byte output is identical to `to_vec`.
    pub fn into_wire_bytes(self) -> Vec<u8> {
        let Self {
            header,
            query,
            mut body,
        } = self;
        let prefix_len = HEADER_SIZE + query.len();
        let body_len = body.len();
        let total = prefix_len + body_len;

        if body.capacity() >= total {
            // Reuse the body allocation: grow in place, shift the body bytes
            // to the back, then overwrite the prefix with header + query.
            body.resize(total, 0);
            if body_len > 0 {
                body.copy_within(0..body_len, prefix_len);
            }
            body[..HEADER_SIZE].copy_from_slice(&header.encode());
            if !query.is_empty() {
                body[HEADER_SIZE..prefix_len].copy_from_slice(&query);
            }
            body
        } else {
            // No room in body for the prefix: fall back to a fresh allocation.
            // Same cost as `to_vec`, but consumes self so the original query
            // and body buffers are released as soon as the wire bytes are
            // produced.
            let mut out = Vec::with_capacity(total);
            out.extend_from_slice(&header.encode());
            if !query.is_empty() {
                out.extend_from_slice(&query);
            }
            if body_len > 0 {
                out.append(&mut body);
            }
            out
        }
    }

    pub fn from_slice(buf: &[u8]) -> Result<Self, RepeError> {
        if buf.len() < HEADER_SIZE {
            return Err(RepeError::InvalidHeaderLength(buf.len()));
        }
        let header = Header::decode(&buf[..HEADER_SIZE])?;
        let expected = HEADER_SIZE + header.query_length as usize + header.body_length as usize;
        if buf.len() < expected {
            return Err(RepeError::BufferTooSmall {
                need: expected,
                have: buf.len(),
            });
        }
        let mut o = HEADER_SIZE;
        let query = buf[o..o + header.query_length as usize].to_vec();
        o += header.query_length as usize;
        let body = buf[o..o + header.body_length as usize].to_vec();
        Self::new(header, query, body)
    }

    pub fn from_slice_exact(buf: &[u8]) -> Result<Self, RepeError> {
        let message = Self::from_slice(buf)?;
        let expected = HEADER_SIZE + message.query.len() + message.body.len();
        if buf.len() != expected {
            return Err(RepeError::LengthMismatch {
                expected: expected as u64,
                got: buf.len() as u64,
            });
        }
        Ok(message)
    }

    pub fn is_error(&self) -> bool {
        self.header.ec != ErrorCode::Ok as u32
    }

    pub fn error_code(&self) -> Option<ErrorCode> {
        ErrorCode::try_from(self.header.ec).ok()
    }

    pub fn error_message_utf8(&self) -> Option<String> {
        if !self.is_error() {
            return None;
        }
        Some(String::from_utf8_lossy(&self.body).into_owned())
    }

    pub fn query_utf8(&self) -> String {
        match self.query_str() {
            Ok(s) => s.to_owned(),
            Err(_) => String::from_utf8_lossy(&self.query).into_owned(),
        }
    }

    pub fn query_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.query)
    }

    pub fn body_utf8(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Decode a JSON body.
    ///
    /// Bounded on the JSON *read* half alone: a body being decoded is never
    /// written, and a type used only with one format should not have to be
    /// declared for the other. The lifetime is the message's, so a `T` whose
    /// fields borrow decodes without copying out of the body.
    pub fn json_body<'a, T>(&'a self) -> Result<T, RepeError>
    where
        T: structio::json::Read<'a> + Default,
    {
        self.require_body_format(BodyFormat::Json)?;
        let text = std::str::from_utf8(&self.body).map_err(|err| {
            RepeError::Json(structio::Error::new(
                structio::ErrorCode::InvalidUtf8,
                err.valid_up_to(),
            ))
        })?;
        let mut value = T::default();
        structio::json::read_into_with::<crate::structs::WirePolicy, _>(&mut value, text)
            .map_err(RepeError::Json)?;
        Ok(value)
    }

    /// Decode a BEVE body. The mirror of [`json_body`](Self::json_body), and
    /// bounded the same way.
    pub fn beve_body<'a, T>(&'a self) -> Result<T, RepeError>
    where
        T: structio::beve::Read<'a> + Default,
    {
        self.require_body_format(BodyFormat::Beve)?;
        let mut value = T::default();
        structio::beve::read_into_with::<crate::structs::WirePolicy, _>(&mut value, &self.body)
            .map_err(RepeError::Beve)?;
        Ok(value)
    }

    /// Decode a BEVE typed-numeric-array body into a `Vec<T>`.
    ///
    /// The decode counterpart of [`MessageBuilder::body_typed_slice`], named for
    /// the payload it is for. It is `beve_body::<Vec<T>>()` and nothing more:
    /// structio has one reader, and it already takes the bulk path for a
    /// `Vec<T: NumericBytes>` — one bounds-checked `copy_nonoverlapping` on a
    /// little-endian target rather than a per-element walk. Under `beve` this
    /// dispatched to a separate reader, which is why it was a separate method.
    ///
    /// The `NumericBytes` bound selects nothing; it is kept so the name cannot
    /// be used for a body that would not take the bulk path after all.
    pub fn decode_typed_slice<T>(&self) -> Result<Vec<T>, RepeError>
    where
        T: structio::beve::NumericBytes + for<'de> structio::beve::Read<'de> + Default,
    {
        self.beve_body()
    }

    /// Decode a BEVE complex-array body into a `Vec<Complex<T>>`.
    ///
    /// The complex counterpart of [`decode_typed_slice`](Self::decode_typed_slice)
    /// and the decode counterpart of
    /// [`MessageBuilder::body_complex_slice`]. Same error contract.
    pub fn decode_complex_slice<T>(&self) -> Result<Vec<structio::Complex<T>>, RepeError>
    where
        structio::Complex<T>:
            structio::beve::NumericBytes + for<'de> structio::beve::Read<'de> + Default,
    {
        self.beve_body()
    }

    /// Decode the body into `R` using whichever format the frame header
    /// declares.
    ///
    /// The counterpart to [`json_body`](Self::json_body) and
    /// [`beve_body`](Self::beve_body), which each demand one format and reject
    /// the other. This is what a *client* wants: it chose the request's format,
    /// but the response's is the server's to pick, so the decoder follows the
    /// header rather than an assumption.
    ///
    /// [`BodyFormat::Utf8`] is parsed as JSON, which is what that code has
    /// always meant on this path. [`BodyFormat::RawBinary`] has no structured
    /// decode and is reported as [`RepeError::UnexpectedBodyFormat`], as is a
    /// format code this build does not recognize — `BodyFormat` is
    /// `#[non_exhaustive]`, so the spec can add one, and an unknown name
    /// decodes no better than an unknown number.
    pub fn decode_body<R: crate::structs::ServableOwned>(&self) -> Result<R, RepeError> {
        let mut value = R::default();
        match BodyFormat::try_from(self.header.body_format) {
            Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => {
                let text = std::str::from_utf8(&self.body).map_err(|err| {
                    RepeError::Json(structio::Error::new(
                        structio::ErrorCode::InvalidUtf8,
                        err.valid_up_to(),
                    ))
                })?;
                structio::json::read_into_with::<crate::structs::WirePolicy, _>(&mut value, text)
                    .map_err(RepeError::Json)?;
            }
            Ok(BodyFormat::Beve) => {
                structio::beve::read_into_with::<crate::structs::WirePolicy, _>(
                    &mut value, &self.body,
                )
                .map_err(RepeError::Beve)?;
            }
            Ok(_) | Err(_) => {
                return Err(RepeError::UnexpectedBodyFormat {
                    expected: BodyFormat::Json,
                    got: self.header.body_format,
                });
            }
        }
        Ok(value)
    }

    /// `Ok(())` if the body's format matches `expected`, else
    /// [`RepeError::UnexpectedBodyFormat`]. The shared format guard for the
    /// body decoders ([`json_body`](Self::json_body), [`beve_body`](Self::beve_body),
    /// [`decode_typed_slice`](Self::decode_typed_slice), and
    /// [`decode_complex_slice`](Self::decode_complex_slice)), so a wrong-format
    /// body produces one structured error shape across all of them.
    fn require_body_format(&self, expected: BodyFormat) -> Result<(), RepeError> {
        if self.header.body_format == expected as u16 {
            Ok(())
        } else {
            Err(RepeError::UnexpectedBodyFormat {
                expected,
                got: self.header.body_format,
            })
        }
    }
}

/// Borrowing view over a serialized REPE message.
///
/// Unlike [`Message::from_slice`], which copies the query and body out of the
/// caller's buffer, `MessageView` keeps both as borrowed slices of the input.
/// Useful when a large body (e.g. a multi-MiB chunk) will be read as a
/// `&'a [u8]` or borrowed with [`structio::beve_slice_ref`], so the bulk payload
/// stays borrowed end-to-end.
///
/// The `header` is decoded by value because it's only 48 bytes and downstream
/// code typically wants the parsed fields rather than the raw header bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageView<'a> {
    pub header: Header,
    pub query: &'a [u8],
    pub body: &'a [u8],
}

impl<'a> MessageView<'a> {
    /// Parse a `MessageView` from `buf`. The returned view borrows from `buf`;
    /// no copies are made.
    pub fn from_slice(buf: &'a [u8]) -> Result<Self, RepeError> {
        if buf.len() < HEADER_SIZE {
            return Err(RepeError::InvalidHeaderLength(buf.len()));
        }
        let header = Header::decode(&buf[..HEADER_SIZE])?;
        let expected = HEADER_SIZE + header.query_length as usize + header.body_length as usize;
        if buf.len() < expected {
            return Err(RepeError::BufferTooSmall {
                need: expected,
                have: buf.len(),
            });
        }
        let q_start = HEADER_SIZE;
        let q_end = q_start + header.query_length as usize;
        let b_end = q_end + header.body_length as usize;
        Ok(Self {
            header,
            query: &buf[q_start..q_end],
            body: &buf[q_end..b_end],
        })
    }

    /// Like [`from_slice`](Self::from_slice) but rejects trailing bytes: errors
    /// if `buf` is longer than the framed message. The borrowing counterpart of
    /// [`Message::from_slice_exact`], for transports (e.g. a WebSocket binary
    /// frame) that carry exactly one message per buffer.
    pub fn from_slice_exact(buf: &'a [u8]) -> Result<Self, RepeError> {
        let view = Self::from_slice(buf)?;
        let expected = HEADER_SIZE + view.query.len() + view.body.len();
        if buf.len() != expected {
            return Err(RepeError::LengthMismatch {
                expected: expected as u64,
                got: buf.len() as u64,
            });
        }
        Ok(view)
    }

    /// View the query as a `&str`. Errors if the query is not valid UTF-8.
    pub fn query_str(&self) -> Result<&'a str, std::str::Utf8Error> {
        std::str::from_utf8(self.query)
    }

    /// Copy this borrowed view into an owned [`Message`], allocating the query
    /// and body. Used as the fallback for [`HandlerErased::handle_view`] when a
    /// handler has not overridden the borrowing path.
    ///
    /// [`HandlerErased::handle_view`]: crate::server::HandlerErased::handle_view
    pub fn to_message(&self) -> Message {
        Message {
            header: self.header,
            query: self.query.to_vec(),
            body: self.body.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_body_non_json_errors() {
        let msg = Message::builder()
            .id(1)
            .query_str("/q")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("not json")
            .build();
        let err = msg.json_body::<Pair>().unwrap_err();
        assert!(
            matches!(
                err,
                RepeError::UnexpectedBodyFormat {
                    expected: BodyFormat::Json,
                    ..
                }
            ),
            "expected UnexpectedBodyFormat, got {err:?}"
        );
    }

    #[derive(Debug, Default, PartialEq)]
    struct Pair {
        a: i32,
        b: i32,
    }
    structio::object!(Pair { a, b });

    /// Stands in for the ad-hoc `json!({"ok": true})` bodies these tests used to
    /// build. There is no tree to construct one from now: a body is a type.
    #[derive(Debug, Default, PartialEq)]
    struct Ok_ {
        ok: bool,
    }
    structio::object!(Ok_ { ok });

    #[test]
    fn create_response_json_and_utf8() {
        let req = Message::builder()
            .id(5)
            .query_str("/sum")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&Pair { a: 1, b: 2 })
            .build();

        // JSON body
        let resp_json = create_response(&req, &Ok_ { ok: true }, BodyFormat::Json);
        assert_eq!(resp_json.header.id, 5);
        assert_eq!(resp_json.header.ec, ErrorCode::Ok as u32);
        assert_eq!(resp_json.header.body_format, BodyFormat::Json as u16);

        // UTF8 body (stringified JSON)
        let resp_utf8 = create_response(&req, &vec![1, 2, 3], BodyFormat::Utf8);
        assert_eq!(resp_utf8.header.body_format, BodyFormat::Utf8 as u16);
        assert!(std::str::from_utf8(&resp_utf8.body).unwrap().contains("1"));
    }

    #[test]
    fn message_from_slice_truncated_returns_buffer_too_small() {
        let msg = Message::builder()
            .id(10)
            .query_str("/a")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("payload")
            .build();
        let mut bytes = msg.to_vec();
        let full_len = bytes.len();
        bytes.truncate(bytes.len() - 1);
        match Message::from_slice(&bytes).unwrap_err() {
            RepeError::BufferTooSmall { need, have } => {
                assert_eq!(need, full_len);
                assert_eq!(have, full_len - 1);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn message_from_slice_exact_rejects_trailing_bytes() {
        let msg = Message::builder()
            .id(11)
            .query_str("/a")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("payload")
            .build();
        let mut bytes = msg.to_vec();
        bytes.extend_from_slice(&[0xAA, 0xBB]);

        match Message::from_slice_exact(&bytes).unwrap_err() {
            RepeError::LengthMismatch { expected, got } => {
                assert_eq!(expected, msg.to_vec().len() as u64);
                assert_eq!(got, bytes.len() as u64);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn create_response_beve_serializes_body() {
        let req = Message::builder()
            .id(3)
            .query_str("/noop")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("{}");
        let req = req.build();

        let resp = create_response(&req, &Ok_ { ok: true }, BodyFormat::Beve);
        assert_eq!(resp.header.id, req.header.id);
        assert_eq!(resp.query, req.query);
        assert!(!resp.body.is_empty());
        assert_eq!(resp.header.body_format, BodyFormat::Beve as u16);

        let value: Ok_ = resp.beve_body().unwrap();
        assert_eq!(value, Ok_ { ok: true });
    }

    #[test]
    fn unstamped_plus_stamp_equals_create_response() {
        let req = Message::builder()
            .id(5)
            .query_str("/sum")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&Pair { a: 1, b: 2 })
            .build();
        let value = Ok_ { ok: true };

        // The echoing path.
        let echoed = create_response(&req, &value, BodyFormat::Json);

        // The boundary path: build query-less, then stamp the request query in.
        // The off-reader boundary owns its request, so it moves the buffer in
        // (`Cow::Owned`, no copy).
        let mut staged = create_response_unstamped(&req, &value, BodyFormat::Json);
        assert!(staged.query.is_empty(), "handler leaves the query empty");
        assert_eq!(staged.header.query_length, 0);
        stamp_response_query(&mut staged, Cow::Owned(req.query.clone()));

        // Byte-for-byte identical to the echoing path, including the header.
        assert_eq!(staged, echoed);
        assert_eq!(staged.query, req.query);
        assert_eq!(staged.header.length, echoed.header.length);

        // Stamp is a no-op once a query is present (e.g. an error response) and
        // when the request query is empty; a borrowed query is then never copied.
        let before = staged.clone();
        stamp_response_query(&mut staged, Cow::Borrowed(&b"/other"[..]));
        assert_eq!(staged, before, "stamp must not overwrite an existing query");
    }

    #[test]
    fn error_response_view_matches_owned_query_format() {
        // The borrowing error path must frame byte-identically to the owned
        // (WebSocket) path. Both leave query_format at the create_error_message
        // default (RawBinary), so a JsonPointer request still yields the same
        // error frame regardless of transport.
        let req = Message::builder()
            .id(9)
            .query_str("/missing")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("{}")
            .build();
        let owned = create_error_response_like(&req, ErrorCode::MethodNotFound, "nope");

        let bytes = req.to_vec();
        let view = MessageView::from_slice(&bytes).unwrap();
        let viewed = create_error_response_unstamped_view(&view, ErrorCode::MethodNotFound, "nope");

        assert_eq!(viewed.header.query_format, owned.header.query_format);
        assert_eq!(viewed.header.id, owned.header.id);
        assert_eq!(viewed.header.ec, owned.header.ec);
        assert_eq!(viewed.body, owned.body);
    }

    #[test]
    fn response_echo_query_prefers_a_handler_set_query() {
        // Empty response query -> echo the request query (the common case).
        let empty = Message::builder().id(1).build();
        assert_eq!(response_echo_query(&empty, b"/req"), b"/req");

        // A query the handler set itself is preserved, not overwritten.
        let custom = Message::builder()
            .id(1)
            .query_str("/handler-set")
            .query_format(QueryFormat::JsonPointer)
            .build();
        assert_eq!(response_echo_query(&custom, b"/req"), b"/handler-set");
    }

    #[test]
    fn body_beve_roundtrip() {
        #[derive(Debug, Default, PartialEq)]
        struct Data {
            x: i32,
            y: String,
        }
        structio::object!(Data { x, y });

        let msg = Message::builder()
            .id(7)
            .query_str("/d")
            .query_format(QueryFormat::JsonPointer)
            .body_beve(&Data {
                x: 10,
                y: "ok".into(),
            })
            .build();

        assert_eq!(msg.header.body_format, BodyFormat::Beve as u16);
        let decoded: Data = msg.beve_body().unwrap();
        assert_eq!(
            decoded,
            Data {
                x: 10,
                y: "ok".into()
            }
        );
    }

    #[test]
    fn message_view_from_slice_borrows() {
        let msg = Message::builder()
            .id(7)
            .query_str("/echo")
            .query_format(QueryFormat::JsonPointer)
            .body_bytes(vec![1u8, 2, 3, 4])
            .body_format(BodyFormat::RawBinary)
            .build();
        let bytes = msg.to_vec();

        let view = MessageView::from_slice(&bytes).unwrap();
        assert_eq!(view.header, msg.header);
        assert_eq!(view.query, msg.query.as_slice());
        assert_eq!(view.body, msg.body.as_slice());

        // Verify the slices point inside `bytes`, not into a fresh allocation.
        let bytes_start = bytes.as_ptr() as usize;
        let bytes_end = bytes_start + bytes.len();
        let q_addr = view.query.as_ptr() as usize;
        let b_addr = view.body.as_ptr() as usize;
        assert!(q_addr >= bytes_start && q_addr <= bytes_end);
        assert!(b_addr >= bytes_start && b_addr <= bytes_end);

        assert_eq!(view.query_str().unwrap(), "/echo");
    }

    #[test]
    fn message_view_truncated_returns_buffer_too_small() {
        let msg = Message::builder()
            .id(1)
            .query_str("/x")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("payload")
            .build();
        let mut bytes = msg.to_vec();
        let full_len = bytes.len();
        bytes.truncate(bytes.len() - 1);
        match MessageView::from_slice(&bytes).unwrap_err() {
            RepeError::BufferTooSmall { need, have } => {
                assert_eq!(need, full_len);
                assert_eq!(have, full_len - 1);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn write_to_matches_to_vec() {
        let msg = Message::builder()
            .id(42)
            .query_str("/collect/file_chunk")
            .query_format(QueryFormat::JsonPointer)
            .body_bytes(vec![0xDEu8; 1024])
            .body_format(BodyFormat::Beve)
            .build();

        let expected = msg.to_vec();
        let mut got = Vec::new();
        msg.write_to(&mut got).expect("write_to");
        assert_eq!(got, expected);
        assert_eq!(msg.serialized_len(), expected.len());
    }

    #[test]
    fn write_to_handles_empty_query_and_body() {
        let msg = Message::builder().id(1).build();
        let expected = msg.to_vec();
        let mut got = Vec::new();
        msg.write_to(&mut got).expect("write_to");
        assert_eq!(got, expected);
        assert_eq!(msg.serialized_len(), expected.len());
    }

    #[test]
    fn into_wire_bytes_matches_to_vec_slow_path() {
        // Body allocated with cap == len, so into_wire_bytes hits the fresh-
        // allocation fallback. Output must still equal to_vec.
        let msg = Message::builder()
            .id(42)
            .query_str("/collect/file_chunk")
            .query_format(QueryFormat::JsonPointer)
            .body_bytes(vec![0xDEu8; 1024])
            .body_format(BodyFormat::Beve)
            .build();
        let expected = msg.to_vec();
        let got = msg.into_wire_bytes();
        assert_eq!(got, expected);
    }

    #[test]
    fn into_wire_bytes_matches_to_vec_fast_path() {
        // Body pre-allocated with prefix room so into_wire_bytes takes the
        // in-place fast path. Output must still equal to_vec.
        let query = b"/collect/file_chunk";
        let body_len = 4096;
        let mut body = Vec::with_capacity(HEADER_SIZE + query.len() + body_len);
        body.resize(body_len, 0xAB);
        let msg = Message::builder()
            .id(42)
            .query_bytes(query.to_vec())
            .query_format(QueryFormat::JsonPointer)
            .body_bytes(body)
            .body_format(BodyFormat::Beve)
            .build();
        let expected = msg.clone().to_vec();
        let got = msg.into_wire_bytes();
        assert_eq!(got, expected);
    }

    #[test]
    fn into_wire_bytes_handles_empty_query_and_body() {
        let msg = Message::builder().id(1).build();
        let expected = msg.to_vec();
        let got = msg.into_wire_bytes();
        assert_eq!(got, expected);
    }

    #[test]
    fn into_wire_bytes_handles_empty_body_only() {
        let msg = Message::builder()
            .id(2)
            .query_str("/notify")
            .query_format(QueryFormat::JsonPointer)
            .build();
        let expected = msg.to_vec();
        let got = msg.into_wire_bytes();
        assert_eq!(got, expected);
    }

    #[test]
    fn into_wire_bytes_handles_empty_query_only() {
        let msg = Message::builder()
            .id(3)
            .body_bytes(vec![1u8, 2, 3])
            .body_format(BodyFormat::RawBinary)
            .build();
        let expected = msg.clone().to_vec();
        let got = msg.into_wire_bytes();
        assert_eq!(got, expected);
    }

    #[test]
    fn into_wire_bytes_fast_path_does_not_reallocate() {
        // Confirm that when the body has room for the prefix, the resulting
        // Vec reuses the body's original allocation (same data pointer).
        let query = b"/q";
        let body_len = 256;
        let total = HEADER_SIZE + query.len() + body_len;
        let mut body = Vec::with_capacity(total);
        body.resize(body_len, 0x42);
        let body_ptr = body.as_ptr();
        let msg = Message::builder()
            .id(1)
            .query_bytes(query.to_vec())
            .query_format(QueryFormat::JsonPointer)
            .body_bytes(body)
            .body_format(BodyFormat::RawBinary)
            .build();
        let wire = msg.into_wire_bytes();
        assert_eq!(wire.len(), total);
        assert_eq!(
            wire.as_ptr(),
            body_ptr,
            "fast path should reuse body buffer"
        );
    }

    #[test]
    fn typed_slice_body_reserves_wire_prefix_headroom() {
        // With the query set before the body, `body_typed_slice` must reserve
        // `HEADER_SIZE + query.len()` of spare capacity after the encoded
        // payload so `into_wire_bytes` reuses the body allocation (no fresh
        // frame buffer). Verified by pointer identity across the framing.
        let query = b"/sensors/raw";
        let data: Vec<f64> = (0..512).map(|i| i as f64 * 0.25).collect();
        let msg = Message::builder()
            .query_bytes(query.to_vec())
            .query_format(QueryFormat::JsonPointer)
            .body_typed_slice(&data)
            .build();
        let total = HEADER_SIZE + query.len() + msg.body.len();
        let body_ptr = msg.body.as_ptr();
        let expected = msg.clone().to_vec();
        let wire = msg.into_wire_bytes();
        assert_eq!(wire, expected, "wire bytes must equal to_vec");
        assert_eq!(wire.len(), total);
        assert_eq!(
            wire.as_ptr(),
            body_ptr,
            "typed-slice body headroom should let into_wire_bytes reuse the body buffer"
        );
    }

    #[test]
    fn complex_slice_body_reserves_wire_prefix_headroom() {
        let query = b"/spectra/iq";
        let data: Vec<structio::Complex<f64>> = (0..256)
            .map(|i| structio::Complex {
                re: i as f64,
                im: -(i as f64) * 0.5,
            })
            .collect();
        let msg = Message::builder()
            .query_bytes(query.to_vec())
            .query_format(QueryFormat::JsonPointer)
            .body_complex_slice(&data)
            .build();
        let total = HEADER_SIZE + query.len() + msg.body.len();
        let body_ptr = msg.body.as_ptr();
        let expected = msg.clone().to_vec();
        let wire = msg.into_wire_bytes();
        assert_eq!(wire, expected, "wire bytes must equal to_vec");
        assert_eq!(wire.len(), total);
        assert_eq!(
            wire.as_ptr(),
            body_ptr,
            "complex-slice body headroom should let into_wire_bytes reuse the body buffer"
        );
    }

    #[test]
    fn typed_slice_body_query_after_body_falls_back_to_fresh_frame() {
        // The headroom is sized from the query length known at `body_typed_slice`
        // time. With the query set *afterward*, the reserved prefix covers only
        // the header, so a non-empty query no longer fits and `into_wire_bytes`
        // takes the slow path (fresh frame). This pins the documented fallback:
        // bytes still equal `to_vec`, but the body allocation is not reused.
        let query = b"/sensors/raw";
        let data: Vec<f64> = (0..512).map(|i| i as f64 * 0.25).collect();
        let msg = Message::builder()
            .body_typed_slice(&data)
            .query_bytes(query.to_vec())
            .query_format(QueryFormat::JsonPointer)
            .build();
        let body_ptr = msg.body.as_ptr();
        let expected = msg.clone().to_vec();
        let wire = msg.into_wire_bytes();
        assert_eq!(wire, expected, "wire bytes must equal to_vec");
        assert_ne!(
            wire.as_ptr(),
            body_ptr,
            "query set after body should not fit the reserved prefix; expected a fresh frame"
        );
    }

    #[test]
    fn beve_body_non_beve_errors() {
        let msg = Message::builder()
            .id(2)
            .query_str("/a")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("text")
            .build();

        let err = msg.beve_body::<Pair>().unwrap_err();
        assert!(
            matches!(
                err,
                RepeError::UnexpectedBodyFormat {
                    expected: BodyFormat::Beve,
                    ..
                }
            ),
            "expected UnexpectedBodyFormat, got {err:?}"
        );
    }

    #[test]
    fn builder_format_code_methods_set_explicit_codes() {
        let msg = Message::builder()
            .id(9)
            .query_str("/raw")
            .query_format_code(0x7777)
            .body_bytes(vec![1, 2, 3])
            .body_format_code(0x8888)
            .build();

        assert_eq!(msg.header.query_format, 0x7777);
        assert_eq!(msg.header.body_format, 0x8888);
    }

    /// Copy `frame` into a genuinely 8-aligned backing buffer (via `Vec<u64>`) and
    /// return it, so a test can validate the zero-copy borrow at a known-aligned
    /// base address.
    fn into_aligned_buffer(frame: &[u8]) -> Vec<u64> {
        // `+ 1` word guarantees room even when `frame.len()` is a multiple of 8
        // (then `words * 8 == frame.len() + 8`), so the byte view always fits.
        let words = frame.len() / 8 + 1;
        let mut backing: Vec<u64> = vec![0; words];
        // SAFETY: `backing` is 8-aligned and large enough to hold `frame`.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(backing.as_mut_ptr() as *mut u8, words * 8) };
        bytes[..frame.len()].copy_from_slice(frame);
        backing
    }

    fn aligned_request(id: u64, path: &str, data: &[f64]) -> Message {
        Message::builder()
            .id(id)
            .query_str(path)
            .query_format(QueryFormat::JsonPointer)
            .body_aligned_typed_slice(data)
            .build()
    }

    #[test]
    fn aligned_body_builder_header_and_payload_round_trip() {
        let data: Vec<f64> = (0..300).map(|i| i as f64 * 0.5).collect();
        let frame = aligned_request(42, "/scale", &data).to_vec();

        // The header parses, the lengths add up, and the body is a valid aligned
        // typed array that decodes (owned, always works) back to the input.
        let view = MessageView::from_slice(&frame).expect("frame parses");
        assert_eq!(view.header.id, 42);
        assert_eq!(view.header.query_format, QueryFormat::JsonPointer as u16);
        assert_eq!(view.header.body_format, BodyFormat::Beve as u16);
        assert_eq!(view.query, b"/scale");
        assert_eq!(view.header.length as usize, frame.len());
        let back = structio::from_beve::<Vec<f64>>(view.body).expect("owned decode");
        assert_eq!(back, data);
    }

    #[test]
    fn aligned_body_payload_is_borrowable_when_buffer_is_aligned() {
        // Validate the padding math end to end: when the whole frame sits at an
        // 8-aligned base address, the f64 payload is aligned in memory and the
        // zero-copy borrow succeeds. A 7-byte query makes the unpadded payload
        // offset odd, so a correct borrow can only come from the inserted padding.
        let data: Vec<f64> = (0..257).map(|i| 1.0 / (i as f64 + 1.0)).collect();
        let frame = aligned_request(1, "/abcdef", &data).to_vec();

        let backing = into_aligned_buffer(&frame);
        let bytes =
            unsafe { std::slice::from_raw_parts(backing.as_ptr() as *const u8, frame.len()) };

        let view = MessageView::from_slice(bytes).expect("frame parses");
        let borrowed: &[f64] =
            structio::beve_slice_ref::<f64>(view.body).expect("zero-copy borrow");
        assert_eq!(borrowed, data.as_slice());
    }

    #[test]
    fn aligned_body_survives_into_wire_bytes() {
        // `into_wire_bytes` shifts the body to `HEADER_SIZE + query.len()`, which is
        // exactly the offset `body_aligned_typed_slice` padded for, so the alignment
        // is preserved through that outbound path too (not just `to_vec`).
        let data: Vec<f64> = (0..130).map(|i| i as f64 - 64.0).collect();
        let frame = aligned_request(7, "/abcdef", &data).into_wire_bytes();
        assert_eq!(
            structio::from_beve::<Vec<f64>>(MessageView::from_slice(&frame).unwrap().body).unwrap(),
            data
        );

        let backing = into_aligned_buffer(&frame);
        let bytes =
            unsafe { std::slice::from_raw_parts(backing.as_ptr() as *const u8, frame.len()) };
        let view = MessageView::from_slice(bytes).expect("frame parses");
        let borrowed: &[f64] =
            structio::beve_slice_ref::<f64>(view.body).expect("zero-copy borrow");
        assert_eq!(borrowed, data.as_slice());
    }
}

#[derive(Default)]
pub struct MessageBuilder {
    id: u64,
    query: Vec<u8>,
    body: Vec<u8>,
    query_format: u16,
    body_format: u16,
    notify: bool,
    ec: u32,
}

impl MessageBuilder {
    pub fn id(mut self, id: u64) -> Self {
        self.id = id;
        self
    }
    pub fn notify(mut self, notify: bool) -> Self {
        self.notify = notify;
        self
    }
    pub fn error_code(mut self, ec: ErrorCode) -> Self {
        self.ec = ec as u32;
        self
    }
    pub fn query_format(mut self, f: QueryFormat) -> Self {
        self.query_format = u16::from(f);
        self
    }
    pub fn query_format_code(mut self, format_code: u16) -> Self {
        self.query_format = format_code;
        self
    }
    pub fn body_format(mut self, f: BodyFormat) -> Self {
        self.body_format = u16::from(f);
        self
    }
    pub fn body_format_code(mut self, format_code: u16) -> Self {
        self.body_format = format_code;
        self
    }
    pub fn query_bytes(mut self, q: impl Into<Vec<u8>>) -> Self {
        self.query = q.into();
        self
    }
    pub fn query_str(mut self, q: &str) -> Self {
        self.query = q.as_bytes().to_vec();
        self
    }
    pub fn body_bytes(mut self, b: impl Into<Vec<u8>>) -> Self {
        self.body = b.into();
        self
    }
    pub fn body_utf8(mut self, s: &str) -> Self {
        self.body = s.as_bytes().to_vec();
        self.body_format = BodyFormat::Utf8 as u16;
        self
    }
    /// Encode `v` as the JSON body.
    ///
    /// Infallible, and that is a change: it returned `Result<Self, RepeError>`
    /// while serde could fail on a value it could not represent. A
    /// [`structio::json::Write`] impl returns `()`, so there is no failure left
    /// to report and no `?` to write at the call site.
    pub fn body_json<T: structio::json::Write + ?Sized>(mut self, v: &T) -> Self {
        self.body = structio::json::to_vec(v);
        self.body_format = BodyFormat::Json as u16;
        self
    }

    /// Encode `v` as the BEVE body. Infallible, for the same reason
    /// [`body_json`](Self::body_json) is.
    pub fn body_beve<T: structio::beve::Write + ?Sized>(mut self, v: &T) -> Self {
        self.body = structio::to_beve(v);
        self.body_format = BodyFormat::Beve as u16;
        self
    }

    /// Encode a contiguous numeric slice as a BEVE typed array via a single bulk
    /// write, and set [`BodyFormat::Beve`].
    ///
    /// This is the whole-body fast path for a high-throughput numeric payload
    /// (`&[f64]`, `&[i32]`, ...). The body bytes are identical to
    /// `body_beve(slice)` — structio writes a numeric sequence as a typed array
    /// either way — and the encode is O(1) in the element count on little-endian
    /// targets (one `copy_nonoverlapping`). Decode the result with
    /// [`Message::decode_typed_slice`].
    ///
    /// The body buffer is allocated with `HEADER_SIZE + query.len()` of spare
    /// capacity reserved after the encoded payload, so that shipping the built
    /// message through [`Message::into_wire_bytes`] reuses this allocation
    /// instead of allocating a fresh frame buffer. This headroom is only
    /// effective when the query is already set on the builder (the common
    /// `.query_*(..).body_typed_slice(..)` order); with the query set afterward
    /// the reserved prefix covers only the header and a non-empty query falls
    /// back to a fresh frame, exactly as before.
    ///
    /// [`body_beve`]: Self::body_beve
    pub fn body_typed_slice<T>(mut self, slice: &[T]) -> Self
    where
        T: structio::beve::NumericBytes + structio::beve::Write,
    {
        let body_len = structio::beve_size(slice);
        let mut body = Vec::with_capacity(body_len + HEADER_SIZE + self.query.len());
        structio::beve::append(slice, &mut body);
        debug_assert_eq!(body.len(), body_len);
        self.body = body;
        self.body_format = BodyFormat::Beve as u16;
        self
    }

    /// Encode a contiguous complex slice as a BEVE complex array via a single
    /// bulk write, and set [`BodyFormat::Beve`].
    ///
    /// The complex counterpart of [`body_typed_slice`]; same O(1)-encode
    /// property, and the same `HEADER_SIZE + query.len()` wire-prefix headroom
    /// for the [`Message::into_wire_bytes`] fast path. Decode with
    /// [`Message::decode_complex_slice`].
    ///
    /// [`body_typed_slice`]: Self::body_typed_slice
    pub fn body_complex_slice<T>(mut self, slice: &[structio::Complex<T>]) -> Self
    where
        structio::Complex<T>: structio::beve::NumericBytes + structio::beve::Write,
    {
        let body_len = structio::beve_size(slice);
        let mut body = Vec::with_capacity(body_len + HEADER_SIZE + self.query.len());
        structio::beve::append(slice, &mut body);
        debug_assert_eq!(body.len(), body_len);
        self.body = body;
        self.body_format = BodyFormat::Beve as u16;
        self
    }

    /// Encode a contiguous numeric slice as a BEVE *aligned* typed array, padded so
    /// the element block lands on an `align_of::<T>()` boundary within the final
    /// wire frame, and set [`BodyFormat::Beve`].
    ///
    /// The zero-copy counterpart of [`body_typed_slice`]: it emits BEVE's aligned
    /// typed-array form (an explicit padding run before the payload) so a
    /// [`Router::with_typed_slice_ref`] server reading the frame into an aligned
    /// buffer can borrow the body as `&[T]` with no element copy. Decode the owned
    /// view with [`Message::decode_typed_slice`] as usual; the borrow happens
    /// server-side.
    ///
    /// The padding is sized for the payload's absolute offset in the frame
    /// (`HEADER_SIZE + query.len()`), so **the query must already be set** when this
    /// is called (the common `.query_*(..).body_aligned_typed_slice(..)` order, and
    /// what the `call_typed_slice_aligned` client helpers do). If the query is set
    /// *after* the body, the padding is computed for the wrong offset and the
    /// payload will not be borrowable in the frame — the server then falls back to a
    /// bulk copy, so the result is still correct, just not zero-copy. The body
    /// buffer carries the same `HEADER_SIZE + query.len()` wire-prefix headroom as
    /// [`body_typed_slice`], and [`Message::into_wire_bytes`] shifts the body to
    /// exactly that offset, so the alignment is preserved through that path too.
    ///
    /// The bytes are *not* a distinct BEVE type: they are the same typed array
    /// [`body_typed_slice`] writes, with padding in front of the payload, and
    /// every reader takes both forms — [`Message::decode_typed_slice`] included.
    /// What the aligned form buys is that [`structio::beve_slice_ref`] can
    /// borrow the payload in place rather than copying it, and a borrow that
    /// cannot be taken declines rather than failing. (Under `beve` these were
    /// two encodings with two readers, which is why this used to say otherwise.)
    ///
    /// [`body_typed_slice`]: Self::body_typed_slice
    /// [`Router::with_typed_slice_ref`]: crate::server::Router::with_typed_slice_ref
    pub fn body_aligned_typed_slice<T>(mut self, slice: &[T]) -> Self
    where
        T: structio::beve::NumericBytes + structio::beve::Write,
    {
        let base_offset = HEADER_SIZE + self.query.len();
        let body_len = structio::beve_size_aligned_after(slice, base_offset);
        // Reserve the `HEADER_SIZE + query.len()` wire-prefix headroom (== base_offset)
        // so `into_wire_bytes` reuses this allocation; its body shift lands the
        // payload at exactly `base_offset`, which is what the padding was sized for.
        let body = Vec::with_capacity(body_len + base_offset);
        // The buffer is the *body*, not the document: the padding has to be
        // measured from where the payload will sit in the finished frame, which
        // is `base_offset` bytes further on. `at` is what says so — appending
        // into this buffer alone would pad against its own length and land the
        // block on the wrong boundary once the header goes in front.
        let mut w = structio::beve::Writer::<structio::Standard>::appending(body)
            .aligned()
            .at(base_offset);
        structio::beve::Write::write(slice, &mut w);
        let body = w.into_vec();
        debug_assert_eq!(body.len(), body_len);
        self.body = body;
        self.body_format = BodyFormat::Beve as u16;
        self
    }

    pub fn build(self) -> Message {
        let mut header = Header::new();
        header.id = self.id;
        header.query_length = self.query.len() as u64;
        header.body_length = self.body.len() as u64;
        header.length = HEADER_SIZE as u64 + header.query_length + header.body_length;
        header.query_format = if self.query_format == 0 {
            QueryFormat::RawBinary as u16
        } else {
            self.query_format
        };
        header.body_format = if self.body_format == 0 {
            BodyFormat::RawBinary as u16
        } else {
            self.body_format
        };
        header.notify = if self.notify { 1 } else { 0 };
        header.ec = self.ec;
        Message {
            header,
            query: self.query,
            body: self.body,
        }
    }
}

pub fn create_error_message(code: ErrorCode, msg: impl AsRef<str>) -> Message {
    let body = msg.as_ref().as_bytes().to_vec();
    Message::builder()
        .error_code(code)
        .body_bytes(body)
        .body_format(BodyFormat::Utf8)
        .build()
}

pub fn create_error_response_like(
    request: &Message,
    code: ErrorCode,
    msg: impl AsRef<str>,
) -> Message {
    create_error_response_for(request.header.id, &request.query, code, msg)
}

/// [`create_error_response_like`] against a request's id and query rather than
/// the whole request.
///
/// Those two fields are all an error response takes from a request; everything
/// else in it comes from [`create_error_message`]. A caller holding them
/// separately — a borrowed [`MessageView`], or a handler that has already split
/// them out — reaches this directly instead of materializing a `Message` for two
/// fields to be read back out of.
pub fn create_error_response_for(
    id: u64,
    query: &[u8],
    code: ErrorCode,
    msg: impl AsRef<str>,
) -> Message {
    let mut err = create_error_message(code, msg.as_ref());
    err.header.id = id;
    err.query = query.to_vec();
    err.header.query_length = err.query.len() as u64;
    err.header.length = HEADER_SIZE as u64 + err.header.query_length + err.header.body_length;
    err
}

pub fn create_response(
    request: &Message,
    result: &(impl crate::structs::ServableWrite + ?Sized),
    body_format: BodyFormat,
) -> Message {
    let builder = response_header_builder(request.header.id, request.header.query_format)
        .query_bytes(request.query.clone());
    finish_response(builder, result, body_format)
}

/// Build a success response that leaves the query **empty**, for the dispatch
/// boundary to fill by moving the request's query in (see
/// [`stamp_response_query`]).
///
/// REPE responses echo the request query verbatim. Rather than have each
/// built-in handler clone the request query into its response, the handlers
/// build the response with this and the transport boundary — which owns the
/// request and drops it right after — moves the query buffer in. That turns the
/// per-response query echo from an allocation + copy into a buffer move.
///
/// Byte-for-byte equivalent to [`create_response`] once
/// [`stamp_response_query`] has run with the originating request's query.
pub(crate) fn create_response_unstamped(
    request: &Message,
    result: &(impl crate::structs::ServableWrite + ?Sized),
    body_format: BodyFormat,
) -> Message {
    let builder = response_header_builder(request.header.id, request.header.query_format);
    finish_response(builder, result, body_format)
}

/// Query-less success response whose body is already encoded.
///
/// The pre-encoded twin of [`create_response_unstamped`], used by the
/// struct-handler dispatch: the handler serializes straight into a buffer via
/// [`ResponseBody`](crate::structs::ResponseBody), so there is no value left to
/// encode and the format is whatever that write settled on.
pub(crate) fn create_body_response_unstamped(
    request: &Message,
    body: Vec<u8>,
    body_format: BodyFormat,
) -> Message {
    response_header_builder(request.header.id, request.header.query_format)
        .body_bytes(body)
        .body_format(body_format)
        .build()
}

/// Echo `request_query` into a response left query-less by
/// [`create_response_unstamped`] / [`create_response_unstamped_view`], fixing up
/// the header lengths.
///
/// A no-op when the response already carries a query (an error response from
/// [`create_error_response_like`], or a custom handler that set its own) or when
/// the request query is empty, so it is safe to call on every dispatched
/// response. Takes a [`Cow`] so the owned/WebSocket off-reader boundary moves its
/// request query buffer straight in (`Cow::Owned`, no copy), while the
/// WebSocket-inline path passes a borrowed slice of the read buffer
/// (`Cow::Borrowed`) that is copied only when the stamp actually commits -- so a
/// custom-query handler that set its own response query pays no allocation.
// Used only by the WebSocket server: the off-reader path moves its owned request
// query in, the inline path copies the borrowed view query. The TCP/async servers
// echo through `response_echo_query` (a pure borrow) instead.
// The predicate mirrors `websocket_server`'s own gate in lib.rs, not just its
// feature: on wasm32 that module is absent even with `websocket` on.
#[cfg_attr(
    not(all(feature = "websocket", not(target_arch = "wasm32"))),
    allow(dead_code)
)]
pub(crate) fn stamp_response_query(response: &mut Message, request_query: Cow<[u8]>) {
    if request_query.is_empty() || !response.query.is_empty() {
        return;
    }
    response.query = request_query.into_owned();
    response.header.query_length = response.query.len() as u64;
    response.header.length =
        HEADER_SIZE as u64 + response.header.query_length + response.header.body_length;
}

/// Pick the query bytes the borrowing dispatch path frames `response` with.
///
/// The borrowing twin of [`stamp_response_query`]: the request query (a borrowed
/// slice of the read buffer) is echoed when the handler left the response query
/// empty, but a query the handler set itself is preserved. Keeps the TCP/async
/// borrowing writers consistent with the owned/WebSocket path on the
/// [`HandlerErased`](crate::server::HandlerErased) custom-response-query
/// contract, instead of unconditionally overwriting with the request query.
// Called only from the TCP and async servers' borrowing writers, both
// `#[cfg(not(target_arch = "wasm32"))]`, so it is dead on a wasm build.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn response_echo_query<'a>(response: &'a Message, request_query: &'a [u8]) -> &'a [u8] {
    if response.query.is_empty() {
        request_query
    } else {
        &response.query
    }
}

/// Query-less success response whose entire body is a BEVE typed numeric array,
/// written via the bulk [`MessageBuilder::body_typed_slice`] path (one
/// `copy_nonoverlapping`, no per-element walk). The typed-slice twin of
/// [`create_response_unstamped`], used by [`Router::with_typed_slice`] so a numeric
/// `Vec<R>` result is framed without serializing element by element.
///
/// [`Router::with_typed_slice`]: crate::server::Router::with_typed_slice
pub(crate) fn create_typed_slice_response_unstamped<
    T: structio::beve::NumericBytes + structio::beve::Write,
>(
    request: &Message,
    result: &[T],
) -> Message {
    response_header_builder(request.header.id, request.header.query_format)
        .body_typed_slice(result)
        .build()
}

/// Borrowing twin of [`create_typed_slice_response_unstamped`]: same bulk typed-array
/// framing, built from a [`MessageView`] for the allocation-free dispatch path.
pub(crate) fn create_typed_slice_response_unstamped_view<
    T: structio::beve::NumericBytes + structio::beve::Write,
>(
    view: &MessageView,
    result: &[T],
) -> Message {
    response_header_builder(view.header.id, view.header.query_format)
        .body_typed_slice(result)
        .build()
}

/// Borrowing twin of [`create_response_unstamped`]: builds the same query-less
/// success response from a [`MessageView`], without materializing an owned
/// request. The view's query is echoed by the writer (e.g.
/// [`write_message_streaming`] with the borrowed query), not by this builder.
pub(crate) fn create_response_unstamped_view(
    view: &MessageView,
    result: &(impl crate::structs::ServableWrite + ?Sized),
    body_format: BodyFormat,
) -> Message {
    let builder = response_header_builder(view.header.id, view.header.query_format);
    finish_response(builder, result, body_format)
}

/// Borrowing, query-less error response from a [`MessageView`]. Mirrors
/// [`create_error_response_like`] exactly: it sets only the request id and leaves
/// the query empty (for the writer to supply from the borrowed view) and the
/// `query_format` at its [`create_error_message`] default, so a view-path error
/// frame is byte-identical to the owned/WebSocket path's.
pub(crate) fn create_error_response_unstamped_view(
    view: &MessageView,
    code: ErrorCode,
    msg: impl AsRef<str>,
) -> Message {
    let mut err = create_error_message(code, msg.as_ref());
    err.header.id = view.header.id;
    err
}

/// Shared response-builder prefix: echo the request id and query format with an
/// `Ok` error code. The caller supplies the query bytes (cloned or moved).
fn response_header_builder(id: u64, query_format: u16) -> MessageBuilder {
    Message::builder()
        .id(id)
        .query_format(QueryFormat::try_from(query_format).unwrap_or(QueryFormat::RawBinary))
        .error_code(ErrorCode::Ok)
}

/// Serialize `result` into the response body per `body_format` and build the
/// message. Shared by [`create_response`], [`create_response_unstamped`], and the
/// `*_view` builders.
fn finish_response(
    builder: MessageBuilder,
    result: &(impl crate::structs::ServableWrite + ?Sized),
    body_format: BodyFormat,
) -> Message {
    let builder = match body_format {
        BodyFormat::Json => builder.body_json(result),
        BodyFormat::Utf8 => {
            let s = structio::json::to_string(result); // convenience: stringify
            builder.body_utf8(&s)
        }
        BodyFormat::RawBinary => {
            // Serialize JSON then treat as bytes; callers wanting true raw should supply bytes.
            builder
                .body_bytes(structio::json::to_vec(result))
                .body_format(BodyFormat::RawBinary)
        }
        BodyFormat::Beve => builder.body_beve(result),
        // A `BodyFormat` this build does not know how to encode. `finish_response`
        // is handed one by a caller that already chose it, so refusing is not an
        // option here — but labelling JSON bytes with a code we did not encode
        // them in is worse than falling back honestly. `body_json` sets the
        // format to match what it wrote.
        _ => builder.body_json(result),
    };
    builder.build()
}
