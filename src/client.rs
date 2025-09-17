use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::io::{read_message, write_message};
use crate::message::Message;
use serde::Serialize;
use serde_json::Value;
use std::io::Write;
use std::io::{BufReader, BufWriter};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct Client {
    stream: TcpStream,
    next_id: Arc<AtomicU64>,
}

impl Client {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(d)
    }
    pub fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_write_timeout(d)
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
        let id = self.next_request_id();
        let msg = Message::builder()
            .id(id)
            .query_str(path.as_ref())
            .query_format(QueryFormat::JsonPointer)
            .body_json(body)?
            .build();
        let mut writer = BufWriter::new(self.stream.try_clone()?);
        write_message(&mut writer, &msg)?;
        writer.flush()?;

        let mut reader = BufReader::new(self.stream.try_clone()?);
        let resp = read_message(&mut reader)?;
        if resp.header.version != REPE_VERSION {
            return Err(RepeError::VersionMismatch(resp.header.version));
        }
        if resp.header.id != id {
            return Err(RepeError::ResponseIdMismatch {
                expected: id,
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
        resp.json_body::<Value>()
    }

    /// Send a JSON-pointer notify request (no response expected).
    pub fn notify_json<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        let id = self.next_request_id();
        let msg = Message::builder()
            .id(id)
            .notify(true)
            .query_str(path.as_ref())
            .query_format(QueryFormat::JsonPointer)
            .body_json(body)?
            .build();
        let mut writer = BufWriter::new(self.stream.try_clone()?);
        write_message(&mut writer, &msg)?;
        writer.flush()?;
        Ok(())
    }
}
