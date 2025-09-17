use crate::async_io::{read_message_async, write_message_async};
use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::message::Message;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, ToSocketAddrs};

pub struct AsyncClient {
    stream: TcpStream,
    next_id: u64,
}

impl AsyncClient {
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Self { stream, next_id: 1 })
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub async fn call_json<P: AsRef<str>, T: Serialize>(
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
        write_message_async(&mut self.stream, &msg).await?;
        self.stream.flush().await?;
        let resp = read_message_async(&mut self.stream).await?;
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

    pub async fn notify_json<P: AsRef<str>, T: Serialize>(
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
        write_message_async(&mut self.stream, &msg).await?;
        self.stream.flush().await?;
        Ok(())
    }
}
