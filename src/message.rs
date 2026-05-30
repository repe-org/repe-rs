use crate::constants::{BodyFormat, ErrorCode, HEADER_SIZE, QueryFormat};
use crate::error::RepeError;
use crate::header::Header;
use beve::{Error as BeveError, from_slice as beve_from_slice, to_vec as beve_to_vec};
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
                expected: HEADER_SIZE as u64 + header.query_length + header.body_length,
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

    pub fn json_body<T: serde::de::DeserializeOwned>(&self) -> Result<T, RepeError> {
        match BodyFormat::try_from(self.header.body_format) {
            Ok(BodyFormat::Json) => Ok(serde_json::from_slice(&self.body)?),
            Ok(BodyFormat::Utf8) | Ok(BodyFormat::RawBinary) | Ok(BodyFormat::Beve) | Err(_) => {
                let io_err =
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "body_format is not JSON");
                Err(RepeError::Json(serde_json::Error::io(io_err)))
            }
        }
    }

    pub fn beve_body<T: serde::de::DeserializeOwned>(&self) -> Result<T, RepeError> {
        match BodyFormat::try_from(self.header.body_format) {
            Ok(BodyFormat::Beve) => Ok(beve_from_slice(&self.body)?),
            Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) | Ok(BodyFormat::RawBinary) | Err(_) => {
                Err(RepeError::from(BeveError::msg("body_format is not BEVE")))
            }
        }
    }
}

/// Borrowing view over a serialized REPE message.
///
/// Unlike [`Message::from_slice`], which copies the query and body out of the
/// caller's buffer, `MessageView` keeps both as borrowed slices of the input.
/// Useful when a large body (e.g. a multi-MiB chunk) will be deserialized with
/// `serde_bytes::Bytes<'a>` so the bulk payload stays borrowed end-to-end.
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
    use serde::{Deserialize, Serialize};

    #[test]
    fn json_body_non_json_errors() {
        let msg = Message::builder()
            .id(1)
            .query_str("/q")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("not json")
            .build();
        let err = msg.json_body::<serde_json::Value>().unwrap_err();
        matches!(err, RepeError::Json(_));
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Pair {
        a: i32,
        b: i32,
    }

    #[test]
    fn create_response_json_and_utf8() {
        let req = Message::builder()
            .id(5)
            .query_str("/sum")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&Pair { a: 1, b: 2 })
            .unwrap()
            .build();

        // JSON body
        let resp_json =
            create_response(&req, serde_json::json!({"ok": true}), BodyFormat::Json).unwrap();
        assert_eq!(resp_json.header.id, 5);
        assert_eq!(resp_json.header.ec, ErrorCode::Ok as u32);
        assert_eq!(resp_json.header.body_format, BodyFormat::Json as u16);

        // UTF8 body (stringified JSON)
        let resp_utf8 =
            create_response(&req, serde_json::json!([1, 2, 3]), BodyFormat::Utf8).unwrap();
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

        let resp =
            create_response(&req, serde_json::json!({"ignored": true}), BodyFormat::Beve).unwrap();
        assert_eq!(resp.header.id, req.header.id);
        assert_eq!(resp.query, req.query);
        assert!(!resp.body.is_empty());
        assert_eq!(resp.header.body_format, BodyFormat::Beve as u16);

        let value: serde_json::Value = resp.beve_body().unwrap();
        assert_eq!(value["ignored"], true);
    }

    #[test]
    fn unstamped_plus_stamp_equals_create_response() {
        let req = Message::builder()
            .id(5)
            .query_str("/sum")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&Pair { a: 1, b: 2 })
            .unwrap()
            .build();
        let value = serde_json::json!({"ok": true});

        // The echoing path.
        let echoed = create_response(&req, &value, BodyFormat::Json).unwrap();

        // The boundary path: build query-less, then stamp the moved query.
        let mut staged = create_response_unstamped(&req, &value, BodyFormat::Json).unwrap();
        assert!(staged.query.is_empty(), "handler leaves the query empty");
        assert_eq!(staged.header.query_length, 0);
        stamp_response_query(&mut staged, req.query.clone());

        // Byte-for-byte identical to the echoing path, including the header.
        assert_eq!(staged, echoed);
        assert_eq!(staged.query, req.query);
        assert_eq!(staged.header.length, echoed.header.length);

        // Stamp is a no-op once a query is present (e.g. an error response) and
        // when the request query is empty.
        let before = staged.clone();
        stamp_response_query(&mut staged, b"/other".to_vec());
        assert_eq!(staged, before, "stamp must not overwrite an existing query");
    }

    #[test]
    fn create_response_propagates_serialization_error() {
        struct Fails;

        impl serde::Serialize for Fails {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("nope"))
            }
        }

        let req = Message::builder()
            .id(12)
            .query_str("/bad")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("{}");
        let req = req.build();

        let err = create_response(&req, Fails, BodyFormat::Json).unwrap_err();
        matches!(err, RepeError::Json(_));
    }

    #[test]
    fn body_beve_roundtrip() {
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Data {
            x: i32,
            y: String,
        }

        let msg = Message::builder()
            .id(7)
            .query_str("/d")
            .query_format(QueryFormat::JsonPointer)
            .body_beve(&Data {
                x: 10,
                y: "ok".into(),
            })
            .unwrap()
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
    fn beve_body_non_beve_errors() {
        let msg = Message::builder()
            .id(2)
            .query_str("/a")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("text")
            .build();

        let err = msg.beve_body::<serde_json::Value>().unwrap_err();
        matches!(err, RepeError::Beve(_));
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
    pub fn body_json<T: serde::Serialize>(mut self, v: &T) -> Result<Self, RepeError> {
        self.body = serde_json::to_vec(v)?;
        self.body_format = BodyFormat::Json as u16;
        Ok(self)
    }

    pub fn body_beve<T: serde::Serialize>(mut self, v: &T) -> Result<Self, RepeError> {
        self.body = beve_to_vec(v)?;
        self.body_format = BodyFormat::Beve as u16;
        Ok(self)
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
    let mut err = create_error_message(code, msg.as_ref());
    err.header.id = request.header.id;
    err.query = request.query.clone();
    err.header.query_length = err.query.len() as u64;
    err.header.length = HEADER_SIZE as u64 + err.header.query_length + err.header.body_length;
    err
}

pub fn create_response(
    request: &Message,
    result: impl serde::Serialize,
    body_format: BodyFormat,
) -> Result<Message, RepeError> {
    let builder = response_header_builder(request.header.id, request.header.query_format)
        .query_bytes(request.query.clone());
    finish_response(builder, result, body_format)
}

/// Like [`create_response`], but consumes `request` so the response reuses the
/// request's `query` buffer (an owned `Vec<u8>` move) instead of cloning it.
///
/// The response always echoes the request query verbatim, so on a dispatch path
/// that owns the request and drops it right after responding, moving the buffer
/// removes the per-response allocation + copy that [`create_response`] pays.
/// The request body has already been consumed to produce `result` by the time
/// this is called, so dropping the rest of `request` costs nothing extra.
pub fn create_response_owned(
    request: Message,
    result: impl serde::Serialize,
    body_format: BodyFormat,
) -> Result<Message, RepeError> {
    let builder = response_header_builder(request.header.id, request.header.query_format)
        .query_bytes(request.query); // moved, not cloned
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
    result: impl serde::Serialize,
    body_format: BodyFormat,
) -> Result<Message, RepeError> {
    let builder = response_header_builder(request.header.id, request.header.query_format);
    finish_response(builder, result, body_format)
}

/// Move `request_query` into a response left query-less by
/// [`create_response_unstamped`], fixing up the header lengths.
///
/// A no-op when the response already carries a query (an error response from
/// [`create_error_response_like`], or a custom handler that set its own) or when
/// the request query is empty, so it is safe to call on every dispatched
/// response. Built-in handlers leave the query empty precisely so this stamp
/// echoes it with a buffer move instead of a clone.
// Used by the owned dispatch boundary, which the WebSocket server takes; the
// TCP/async servers echo the query through the borrowing path instead.
#[cfg_attr(not(feature = "websocket"), allow(dead_code))]
pub(crate) fn stamp_response_query(response: &mut Message, request_query: Vec<u8>) {
    if request_query.is_empty() || !response.query.is_empty() {
        return;
    }
    response.query = request_query;
    response.header.query_length = response.query.len() as u64;
    response.header.length =
        HEADER_SIZE as u64 + response.header.query_length + response.header.body_length;
}

/// Borrowing twin of [`create_response_unstamped`]: builds the same query-less
/// success response from a [`MessageView`], without materializing an owned
/// request. The view's query is echoed by the writer (e.g.
/// [`write_message_streaming`] with the borrowed query), not by this builder.
pub(crate) fn create_response_unstamped_view(
    view: &MessageView,
    result: impl serde::Serialize,
    body_format: BodyFormat,
) -> Result<Message, RepeError> {
    let builder = response_header_builder(view.header.id, view.header.query_format);
    finish_response(builder, result, body_format)
}

/// Borrowing, query-less error response from a [`MessageView`]. Mirrors
/// [`create_error_response_like`] but leaves the query empty for the writer to
/// supply from the borrowed view.
pub(crate) fn create_error_response_unstamped_view(
    view: &MessageView,
    code: ErrorCode,
    msg: impl AsRef<str>,
) -> Message {
    let mut err = create_error_message(code, msg.as_ref());
    err.header.id = view.header.id;
    err.header.query_format = view.header.query_format;
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
/// message. Shared by [`create_response`] and [`create_response_owned`].
fn finish_response(
    builder: MessageBuilder,
    result: impl serde::Serialize,
    body_format: BodyFormat,
) -> Result<Message, RepeError> {
    let builder = match body_format {
        BodyFormat::Json => builder.body_json(&result)?,
        BodyFormat::Utf8 => {
            let s = serde_json::to_string(&result)?; // convenience: stringify
            builder.body_utf8(&s)
        }
        BodyFormat::RawBinary => {
            // Serialize JSON then treat as bytes; callers wanting true raw should supply bytes.
            let v = serde_json::to_vec(&result)?;
            builder.body_bytes(v).body_format(BodyFormat::RawBinary)
        }
        BodyFormat::Beve => builder.body_beve(&result)?,
    };
    Ok(builder.build())
}
