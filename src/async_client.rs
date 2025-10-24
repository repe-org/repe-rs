use crate::async_io::{read_message_async, write_message_async};
use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::message::{Message, MessageBuilder};
use beve::from_slice as beve_from_slice;
use serde::Serialize;
use serde::de::DeserializeOwned;
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
        let resp = self
            .call_with_body(path, |builder| builder.body_json(body))
            .await?;
        resp.json_body::<Value>()
    }

    pub async fn call_typed_json<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_with_body(path, |builder| builder.body_json(body))
            .await?;
        Self::decode_typed_response(&resp)
    }

    pub async fn call_typed_beve<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_with_body(path, |builder| builder.body_beve(body))
            .await?;
        Self::decode_typed_response(&resp)
    }

    pub async fn notify_json<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
            .await
    }

    pub async fn notify_typed_json<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
            .await
    }

    pub async fn notify_typed_beve<P: AsRef<str>, T: Serialize>(
        &mut self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_beve(body))
            .await
    }

    async fn call_with_body<P, F>(&mut self, path: P, body_fn: F) -> Result<Message, RepeError>
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
        write_message_async(&mut self.stream, &msg).await?;
        self.stream.flush().await?;
        let resp = read_message_async(&mut self.stream).await?;
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

    async fn notify_with_body<P, F>(&mut self, path: P, body_fn: F) -> Result<(), RepeError>
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
        write_message_async(&mut self.stream, &msg).await?;
        self.stream.flush().await?;
        Ok(())
    }
}
