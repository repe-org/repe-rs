use crate::constants::{BodyFormat, ErrorCode, QueryFormat, HEADER_SIZE};
use crate::error::RepeError;
use crate::header::Header;

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
        String::from_utf8_lossy(&self.query).into_owned()
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
    fn create_response_beve_leaves_empty_body() {
        let req = Message::builder()
            .id(3)
            .query_str("/noop")
            .query_format(QueryFormat::JsonPointer)
            .body_utf8("{}");
        let req = req.build();

        let resp = create_response(&req, serde_json::json!({"ignored": true}), BodyFormat::Beve)
            .unwrap();
        assert_eq!(resp.header.id, req.header.id);
        assert_eq!(resp.query, req.query);
        assert!(resp.body.is_empty());
        assert_eq!(resp.header.body_format, BodyFormat::RawBinary as u16);
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
    pub fn body_format(mut self, f: BodyFormat) -> Self {
        self.body_format = u16::from(f);
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
    let mut builder = Message::builder()
        .id(request.header.id)
        .query_bytes(request.query.clone())
        .query_format(
            QueryFormat::try_from(request.header.query_format).unwrap_or(QueryFormat::RawBinary),
        )
        .error_code(ErrorCode::Ok);
    builder = match body_format {
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
        BodyFormat::Beve => builder, // not implemented; keep empty
    };
    Ok(builder.build())
}
