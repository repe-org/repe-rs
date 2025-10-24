use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::io::{read_message, write_message};
use crate::message::{Message, MessageBuilder};
use beve::from_slice as beve_from_slice;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::Write;
use std::io::{BufReader, BufWriter};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub struct Client {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    next_id: Arc<AtomicU64>,
}

impl Client {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        let reader_stream = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(reader_stream),
            writer: BufWriter::new(stream),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.reader.get_ref().set_read_timeout(d)
    }
    pub fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.writer.get_ref().set_write_timeout(d)
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a JSON-pointer request with JSON body and receive JSON result.
    pub fn call_json<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        let resp = self.call_with_body(path, |builder| builder.body_json(body))?;
        resp.json_body::<Value>()
    }

    /// Send a JSON-pointer request with JSON body and deserialize the typed result.
    pub fn call_typed_json<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        let resp = self.call_with_body(path, |builder| builder.body_json(body))?;
        Self::decode_typed_response(&resp)
    }

    /// Send a JSON-pointer request with BEVE body and deserialize the typed result.
    pub fn call_typed_beve<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        let resp = self.call_with_body(path, |builder| builder.body_beve(body))?;
        Self::decode_typed_response(&resp)
    }

    /// Send a JSON-pointer notify request (no response expected).
    pub fn notify_json<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
    }

    /// Notify a typed handler with a JSON payload, without waiting for a response.
    pub fn notify_typed_json<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
    }

    /// Notify a typed handler with a BEVE payload, without waiting for a response.
    pub fn notify_typed_beve<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_beve(body))
    }

    fn call_with_body<P, F>(&mut self, path: P, body_fn: F) -> Result<Message, RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        let id = self.next_request_id();
        let builder = Message::builder()
            .id(id)
            .query_str(path.as_ref())
            .query_format(QueryFormat::JsonPointer);
        let msg = body_fn(builder)?.build();
        write_message(&mut self.writer, &msg)?;
        self.writer.flush()?;

        let resp = read_message(&mut self.reader)?;
        Self::validate_response(id, resp)
    }

    fn validate_response(expected_id: u64, resp: Message) -> Result<Message, RepeError> {
        if resp.header.version != REPE_VERSION {
            return Err(RepeError::VersionMismatch(resp.header.version));
        }
        if resp.header.id != expected_id {
            return Err(RepeError::ResponseIdMismatch {
                expected: expected_id,
                got: resp.header.id,
            });
        }
        if resp.header.ec != ErrorCode::Ok as u32 {
            let code = ErrorCode::try_from(resp.header.ec).unwrap_or(ErrorCode::ParseError);
            let msg = resp
                .error_message_utf8()
                .unwrap_or_else(|| code.to_string());
            return Err(RepeError::ServerError { code, message: msg });
        }
        Ok(resp)
    }

    fn notify_with_body<P, F>(&mut self, path: P, body_fn: F) -> Result<(), RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        let id = self.next_request_id();
        let builder = Message::builder()
            .id(id)
            .notify(true)
            .query_str(path.as_ref())
            .query_format(QueryFormat::JsonPointer);
        let msg = body_fn(builder)?.build();
        write_message(&mut self.writer, &msg)?;
        self.writer.flush()?;
        Ok(())
    }

    fn decode_typed_response<R: DeserializeOwned>(resp: &Message) -> Result<R, RepeError> {
        match BodyFormat::try_from(resp.header.body_format) {
            Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => {
                serde_json::from_slice(&resp.body).map_err(RepeError::from)
            }
            Ok(BodyFormat::Beve) => beve_from_slice(&resp.body).map_err(RepeError::from),
            Ok(BodyFormat::RawBinary) => {
                let io_err = std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response body is neither JSON nor BEVE",
                );
                Err(RepeError::Json(serde_json::Error::io(io_err)))
            }
            Err(_) => Err(RepeError::UnknownEnumValue(resp.header.body_format as u64)),
        }
    }
}
