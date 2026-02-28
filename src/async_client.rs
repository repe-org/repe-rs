use crate::async_io::{read_message_async, write_message_async};
use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::message::{Message, MessageBuilder};
use beve::from_slice as beve_from_slice;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncWriteExt;
use tokio::io::{BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinError;
use tokio::time::{Duration, timeout};

type PendingSender = oneshot::Sender<Result<Message, RepeError>>;
type PendingRequests = HashMap<u64, PendingSender>;

#[derive(Clone)]
pub struct AsyncClient {
    inner: Arc<AsyncClientInner>,
}

struct AsyncClientInner {
    writer: Mutex<BufWriter<OwnedWriteHalf>>,
    pending: StdMutex<PendingRequests>,
    next_id: AtomicU64,
    shutdown: StdMutex<Option<oneshot::Sender<()>>>,
}

impl Drop for AsyncClientInner {
    fn drop(&mut self) {
        if let Ok(mut tx) = self.shutdown.lock() {
            if let Some(sender) = tx.take() {
                let _ = sender.send(());
            }
        }
    }
}

enum PendingDispatch {
    Matched {
        sender: PendingSender,
        response: Message,
    },
    Unrecognized {
        got_id: u64,
    },
}

struct PendingRequestGuard {
    inner: Arc<AsyncClientInner>,
    request_id: u64,
    disarmed: bool,
}

impl PendingRequestGuard {
    fn register(inner: &Arc<AsyncClientInner>, request_id: u64, sender: PendingSender) -> Self {
        {
            let mut pending = lock_pending_map(&inner.pending);
            pending.insert(request_id, sender);
        }

        Self {
            inner: Arc::clone(inner),
            request_id,
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }

        let mut pending = lock_pending_map(&self.inner.pending);
        pending.remove(&self.request_id);
    }
}

impl AsyncClient {
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (read_half, write_half) = stream.into_split();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let inner = Arc::new(AsyncClientInner {
            writer: Mutex::new(BufWriter::new(write_half)),
            pending: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            shutdown: StdMutex::new(Some(shutdown_tx)),
        });

        spawn_response_loop(
            BufReader::new(read_half),
            Arc::downgrade(&inner),
            shutdown_rx,
        );

        Ok(Self { inner })
    }

    fn next_request_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn call_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json_with_optional_timeout(path, body, None).await
    }

    /// Send a JSON request and fail if no response arrives before `timeout_duration`.
    pub async fn call_json_with_timeout<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Duration,
    ) -> Result<Value, RepeError> {
        self.call_json_with_optional_timeout(path, body, Some(timeout_duration))
            .await
    }

    pub async fn call_typed_json<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        self.call_typed_json_with_optional_timeout(path, body, None)
            .await
    }

    /// Send a typed JSON request and fail if no response arrives before `timeout_duration`.
    pub async fn call_typed_json_with_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Duration,
    ) -> Result<R, RepeError> {
        self.call_typed_json_with_optional_timeout(path, body, Some(timeout_duration))
            .await
    }

    pub async fn call_typed_beve<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        self.call_typed_beve_with_optional_timeout(path, body, None)
            .await
    }

    /// Send a typed BEVE request and fail if no response arrives before `timeout_duration`.
    pub async fn call_typed_beve_with_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Duration,
    ) -> Result<R, RepeError> {
        self.call_typed_beve_with_optional_timeout(path, body, Some(timeout_duration))
            .await
    }

    /// Send a JSON-pointer request with an empty body and return the full response message.
    ///
    /// This is useful for protocols that use empty-body semantics (for example, registry READs).
    pub async fn call_message<P: AsRef<str>>(&self, path: P) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            None,
            BodyFormat::RawBinary as u16,
            None,
        )
        .await
    }

    /// Send a JSON-pointer request with an empty body, failing if no response arrives before
    /// `timeout_duration`.
    pub async fn call_message_with_timeout<P: AsRef<str>>(
        &self,
        path: P,
        timeout_duration: Duration,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            None,
            BodyFormat::RawBinary as u16,
            Some(timeout_duration),
        )
        .await
    }

    /// Low-level call API that allows custom query/body format codes and optional raw body bytes.
    ///
    /// If `body` is `None`, the request body is empty (`body_length = 0`).
    pub async fn call_with_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(path, query_format, body, body_format, None)
            .await
    }

    /// Timeout variant of [`AsyncClient::call_with_formats`].
    pub async fn call_with_formats_and_timeout<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
        timeout_duration: Duration,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            query_format,
            body,
            body_format,
            Some(timeout_duration),
        )
        .await
    }

    /// Registry helper: send an empty-body request and decode the JSON response.
    pub async fn registry_read<P: AsRef<str>>(&self, path: P) -> Result<Value, RepeError> {
        let resp = self.call_message(path).await?;
        resp.json_body::<Value>()
    }

    /// Registry helper: send an empty-body request and deserialize the JSON response as `R`.
    pub async fn registry_read_typed<P: AsRef<str>, R: DeserializeOwned>(
        &self,
        path: P,
    ) -> Result<R, RepeError> {
        let resp = self.call_message(path).await?;
        resp.json_body::<R>()
    }

    /// Timeout variant of [`AsyncClient::registry_read`].
    pub async fn registry_read_with_timeout<P: AsRef<str>>(
        &self,
        path: P,
        timeout_duration: Duration,
    ) -> Result<Value, RepeError> {
        let resp = self
            .call_message_with_timeout(path, timeout_duration)
            .await?;
        resp.json_body::<Value>()
    }

    /// Timeout variant of [`AsyncClient::registry_read_typed`].
    pub async fn registry_read_typed_with_timeout<P: AsRef<str>, R: DeserializeOwned>(
        &self,
        path: P,
        timeout_duration: Duration,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_message_with_timeout(path, timeout_duration)
            .await?;
        resp.json_body::<R>()
    }

    /// Registry helper: send a JSON body (WRITE semantics for non-function targets).
    pub async fn registry_write_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json(path, body).await
    }

    /// Registry helper: send a JSON body (CALL semantics for function targets).
    pub async fn registry_call_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json(path, body).await
    }

    pub async fn notify_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
            .await
    }

    pub async fn notify_typed_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
            .await
    }

    pub async fn notify_typed_beve<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_beve(body))
            .await
    }

    /// Low-level notify API that allows custom query/body format codes and optional raw body bytes.
    ///
    /// If `body` is `None`, the notify request is sent with an empty body.
    pub async fn notify_with_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError> {
        self.notify_with_message_formats(path, query_format, body, body_format)
            .await
    }

    /// Execute JSON calls in parallel over this single connection and keep request order.
    pub async fn batch_json(
        &self,
        requests: Vec<(String, Value)>,
    ) -> Vec<Result<Value, RepeError>> {
        self.batch_json_inner(requests, None).await
    }

    /// Execute JSON calls in parallel with a per-request timeout.
    pub async fn batch_json_with_timeout(
        &self,
        requests: Vec<(String, Value)>,
        timeout_duration: Duration,
    ) -> Vec<Result<Value, RepeError>> {
        self.batch_json_inner(requests, Some(timeout_duration))
            .await
    }

    async fn batch_json_inner(
        &self,
        requests: Vec<(String, Value)>,
        timeout_duration: Option<Duration>,
    ) -> Vec<Result<Value, RepeError>> {
        let mut workers = Vec::with_capacity(requests.len());

        for (index, (path, body)) in requests.into_iter().enumerate() {
            let client = self.clone();
            workers.push((
                index,
                tokio::spawn(async move {
                    client
                        .call_json_with_optional_timeout(path, &body, timeout_duration)
                        .await
                }),
            ));
        }

        let mut out: Vec<Option<Result<Value, RepeError>>> = std::iter::repeat_with(|| None)
            .take(workers.len())
            .collect();

        for (index, worker) in workers {
            let result = match worker.await {
                Ok(value) => value,
                Err(err) => Err(batch_worker_join_error(err)),
            };
            out[index] = Some(result);
        }

        out.into_iter()
            .map(|entry| entry.unwrap_or_else(|| Err(batch_worker_missing_result_error())))
            .collect()
    }

    async fn call_json_with_optional_timeout<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Option<Duration>,
    ) -> Result<Value, RepeError> {
        let resp = self
            .call_with_body_and_timeout(
                path,
                QueryFormat::JsonPointer as u16,
                timeout_duration,
                |builder| builder.body_json(body),
            )
            .await?;
        resp.json_body::<Value>()
    }

    async fn call_typed_json_with_optional_timeout<
        P: AsRef<str>,
        T: Serialize,
        R: DeserializeOwned,
    >(
        &self,
        path: P,
        body: &T,
        timeout_duration: Option<Duration>,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_with_body_and_timeout(
                path,
                QueryFormat::JsonPointer as u16,
                timeout_duration,
                |builder| builder.body_json(body),
            )
            .await?;
        Self::decode_typed_response(&resp)
    }

    async fn call_typed_beve_with_optional_timeout<
        P: AsRef<str>,
        T: Serialize,
        R: DeserializeOwned,
    >(
        &self,
        path: P,
        body: &T,
        timeout_duration: Option<Duration>,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_with_body_and_timeout(
                path,
                QueryFormat::JsonPointer as u16,
                timeout_duration,
                |builder| builder.body_beve(body),
            )
            .await?;
        Self::decode_typed_response(&resp)
    }

    async fn call_with_body_and_timeout<P, F>(
        &self,
        path: P,
        query_format: u16,
        timeout_duration: Option<Duration>,
        body_fn: F,
    ) -> Result<Message, RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        let id = self.next_request_id();
        let builder = Message::builder()
            .id(id)
            .query_str(path.as_ref())
            .query_format_code(query_format);
        let msg = body_fn(builder)?.build();

        let (sender, receiver) = oneshot::channel();
        let mut pending_guard = PendingRequestGuard::register(&self.inner, id, sender);

        self.write_request(&msg).await?;

        let received = match timeout_duration {
            Some(duration) => match timeout(duration, receiver).await {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => return Err(response_channel_closed_error(id)),
                Err(_) => return Err(request_timeout_error(id, duration)),
            },
            None => match receiver.await {
                Ok(value) => value,
                Err(_) => return Err(response_channel_closed_error(id)),
            },
        };

        let resp = received?;
        pending_guard.disarm();
        Self::validate_response(id, resp)
    }

    async fn write_request(&self, msg: &Message) -> Result<(), RepeError> {
        let mut writer = self.inner.writer.lock().await;
        write_message_async(&mut *writer, msg).await?;
        writer.flush().await?;
        Ok(())
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

    async fn notify_with_body<P, F>(&self, path: P, body_fn: F) -> Result<(), RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        self.notify_with_builder(path, QueryFormat::JsonPointer as u16, body_fn)
            .await
    }

    async fn notify_with_builder<P, F>(
        &self,
        path: P,
        query_format: u16,
        body_fn: F,
    ) -> Result<(), RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        let id = self.next_request_id();
        let builder = Message::builder()
            .id(id)
            .notify(true)
            .query_str(path.as_ref())
            .query_format_code(query_format);
        let msg = body_fn(builder)?.build();
        self.write_request(&msg).await
    }

    async fn call_message_with_formats_and_timeout<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
        timeout_duration: Option<Duration>,
    ) -> Result<Message, RepeError> {
        self.call_with_body_and_timeout(path, query_format, timeout_duration, |builder| {
            let builder = if let Some(bytes) = body {
                builder
                    .body_bytes(bytes.to_vec())
                    .body_format_code(body_format)
            } else {
                builder.body_format_code(body_format)
            };
            Ok(builder)
        })
        .await
    }

    async fn notify_with_message_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError> {
        self.notify_with_builder(path, query_format, |builder| {
            let builder = if let Some(bytes) = body {
                builder
                    .body_bytes(bytes.to_vec())
                    .body_format_code(body_format)
            } else {
                builder.body_format_code(body_format)
            };
            Ok(builder)
        })
        .await
    }
}

fn spawn_response_loop(
    mut reader: BufReader<OwnedReadHalf>,
    inner: std::sync::Weak<AsyncClientInner>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    tokio::spawn(async move {
        loop {
            let response = tokio::select! {
                _ = &mut shutdown_rx => {
                    break;
                }
                read = read_message_async(&mut reader) => {
                    match read {
                        Ok(message) => message,
                        Err(err) => {
                            fail_all_pending(&inner, err).await;
                            break;
                        }
                    }
                }
            };

            let dispatch = {
                let Some(inner_ref) = inner.upgrade() else {
                    break;
                };
                let response_id = response.header.id;
                let matched_sender = {
                    let mut pending = lock_pending_map(&inner_ref.pending);
                    pending.remove(&response_id)
                };

                if let Some(sender) = matched_sender {
                    PendingDispatch::Matched { sender, response }
                } else {
                    PendingDispatch::Unrecognized {
                        got_id: response_id,
                    }
                }
            };

            match dispatch {
                PendingDispatch::Matched { sender, response } => {
                    let _ = sender.send(Ok(response));
                }
                PendingDispatch::Unrecognized { got_id } => {
                    eprintln!("[repe] dropping response for unrecognized request id {got_id}");
                }
            }
        }
    });
}

async fn fail_all_pending(inner: &std::sync::Weak<AsyncClientInner>, err: RepeError) {
    let Some(inner_ref) = inner.upgrade() else {
        return;
    };

    {
        let mut writer = inner_ref.writer.lock().await;
        let _ = writer.shutdown().await;
    }

    let waiters = {
        let mut pending = lock_pending_map(&inner_ref.pending);
        pending.drain().collect::<Vec<_>>()
    };

    for (request_id, sender) in waiters {
        let _ = sender.send(Err(clone_fatal_error_for_waiter(&err, request_id)));
    }
}

fn clone_fatal_error_for_waiter(err: &RepeError, request_id: u64) -> RepeError {
    match err {
        RepeError::VersionMismatch(version) => RepeError::VersionMismatch(*version),
        RepeError::InvalidSpec(spec) => RepeError::InvalidSpec(*spec),
        RepeError::InvalidHeaderLength(length) => RepeError::InvalidHeaderLength(*length),
        RepeError::ReservedNonZero => RepeError::ReservedNonZero,
        RepeError::LengthMismatch { expected, got } => RepeError::LengthMismatch {
            expected: *expected,
            got: *got,
        },
        RepeError::BufferTooSmall { need, have } => RepeError::BufferTooSmall {
            need: *need,
            have: *have,
        },
        RepeError::ResponseIdMismatch { expected, got } => RepeError::ResponseIdMismatch {
            expected: *expected,
            got: *got,
        },
        RepeError::Io(io_err) => RepeError::Io(std::io::Error::new(
            io_err.kind(),
            format!("request {request_id}: {io_err}"),
        )),
        RepeError::Json(json_err) => RepeError::Json(serde_json::Error::io(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("request {request_id}: {json_err}"),
        ))),
        RepeError::Beve(beve_err) => RepeError::Beve(beve::Error::msg(format!(
            "request {request_id}: {beve_err}"
        ))),
        RepeError::UnknownEnumValue(value) => RepeError::UnknownEnumValue(*value),
        RepeError::ServerError { code, message } => RepeError::ServerError {
            code: *code,
            message: message.clone(),
        },
    }
}

fn request_timeout_error(request_id: u64, timeout: Duration) -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::TimedOut,
        format!(
            "request {request_id} timed out after {}ms",
            timeout.as_millis()
        ),
    ))
}

fn response_channel_closed_error(request_id: u64) -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::ConnectionAborted,
        format!("response channel closed for request {request_id}"),
    ))
}

fn batch_worker_join_error(err: JoinError) -> RepeError {
    RepeError::Io(std::io::Error::other(format!("batch worker failed: {err}")))
}

fn batch_worker_missing_result_error() -> RepeError {
    RepeError::Io(std::io::Error::other("batch worker missing result"))
}

fn lock_pending_map(
    pending: &StdMutex<PendingRequests>,
) -> std::sync::MutexGuard<'_, PendingRequests> {
    match pending.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{read_message, write_message};
    use serde_json::json;
    use std::io::{BufReader, BufWriter, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn json_response_for(req: &Message) -> Message {
        Message::builder()
            .id(req.header.id)
            .query_bytes(req.query.clone())
            .query_format(
                QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary),
            )
            .body_json(&json!({"path": req.query_utf8()}))
            .expect("json body")
            .build()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_call_cleans_pending_entry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (first_seen_tx, first_seen_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = BufWriter::new(stream);

            let _first = read_message(&mut reader).unwrap();
            first_seen_tx
                .send(())
                .expect("signal that first request arrived");

            let second = read_message(&mut reader).unwrap();
            let response = json_response_for(&second);
            write_message(&mut writer, &response).unwrap();
            writer.flush().unwrap();
        });

        let client = AsyncClient::connect(addr).await.unwrap();

        let cancelled_client = client.clone();
        let cancelled_call = tokio::spawn(async move {
            cancelled_client
                .call_json("/cancelled", &json!({"n": 1}))
                .await
        });

        tokio::task::spawn_blocking(move || {
            first_seen_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("server should observe the first request");
        })
        .await
        .unwrap();

        cancelled_call.abort();
        let join_err = cancelled_call.await.unwrap_err();
        assert!(join_err.is_cancelled());

        let pending_count = {
            let pending = lock_pending_map(&client.inner.pending);
            pending.len()
        };
        assert_eq!(
            pending_count, 0,
            "cancelled calls must not leak pending entries"
        );

        let out = client
            .call_json_with_timeout("/ok", &json!({"n": 2}), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(out["path"], "/ok");

        tokio::task::spawn_blocking(move || server.join().unwrap())
            .await
            .unwrap();
    }
}
