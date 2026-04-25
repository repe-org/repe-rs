use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::message::{Message, MessageBuilder};
use beve::from_slice as beve_from_slice;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinError;
use tokio::time::{Duration, timeout};
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type PendingSender = oneshot::Sender<Result<Message, RepeError>>;
type PendingRequests = HashMap<u64, PendingSender>;
type WsWriter =
    futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;
type WsReader = futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

#[derive(Clone)]
pub struct WebSocketClient {
    inner: Arc<WebSocketClientInner>,
}

/// Returned by [`WebSocketClient::subscribe_notifies`] when a live
/// subscription already exists.
///
/// "Live" means the prior receiver has not been dropped. To replace a
/// live subscription, call [`WebSocketClient::unsubscribe_notifies`]
/// first. A subscription whose receiver has already been dropped does
/// not block resubscription: `subscribe_notifies` silently replaces
/// the stale slot and returns the new receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadySubscribed;

impl std::fmt::Display for AlreadySubscribed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a notify subscription is already active on this WebSocketClient")
    }
}

impl std::error::Error for AlreadySubscribed {}

struct WebSocketClientInner {
    writer: Mutex<WsWriter>,
    pending: StdMutex<PendingRequests>,
    next_id: AtomicU64,
    /// Unbounded sender for inbound notify messages (any header whose
    /// `notify` flag is non-zero). `None` while no subscriber is
    /// registered; the response loop drops notifies in that case. At
    /// most one live subscriber at a time;
    /// [`WebSocketClient::subscribe_notifies`] returns an
    /// [`AlreadySubscribed`] error if this slot still holds a sender
    /// whose receiver hasn't been dropped.
    notify_tx: StdMutex<Option<mpsc::UnboundedSender<Message>>>,
}

enum PendingDispatch {
    Matched {
        sender: PendingSender,
        response: Message,
    },
    Notify {
        sender: mpsc::UnboundedSender<Message>,
        response: Message,
    },
    DroppedNotify,
    Unrecognized {
        got_id: u64,
    },
}

struct PendingRequestGuard {
    inner: Arc<WebSocketClientInner>,
    request_id: u64,
    disarmed: bool,
}

impl PendingRequestGuard {
    fn register(
        inner: &Arc<WebSocketClientInner>,
        request_id: u64,
        sender: PendingSender,
    ) -> Result<Self, RepeError> {
        {
            let mut pending = lock_pending_map(&inner.pending);
            if pending.contains_key(&request_id) {
                return Err(duplicate_request_id_error(request_id));
            }
            pending.insert(request_id, sender);
        }

        Ok(Self {
            inner: Arc::clone(inner),
            request_id,
            disarmed: false,
        })
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

impl Drop for WebSocketClient {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let inner = Arc::clone(&self.inner);
            handle.spawn(async move {
                let _ = close_writer(&inner).await;
            });
        }
    }
}

impl WebSocketClient {
    pub async fn connect(url: &str) -> std::io::Result<Self> {
        let (stream, _response) = connect_async(url).await.map_err(websocket_connect_error)?;
        let (writer, reader) = stream.split();
        let inner = Arc::new(WebSocketClientInner {
            writer: Mutex::new(writer),
            pending: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            notify_tx: StdMutex::new(None),
        });

        spawn_response_loop(reader, Arc::downgrade(&inner));

        Ok(Self { inner })
    }

    /// Subscribe to inbound notify messages (server-pushed messages with
    /// the `notify` header flag set).
    ///
    /// Returns an unbounded receiver that yields raw [`Message`]s; the
    /// application is responsible for decoding bodies.
    ///
    /// At most one subscriber may be active at a time. If a live
    /// subscription already exists, this method returns
    /// [`AlreadySubscribed`] without disturbing the existing receiver;
    /// the caller must explicitly call
    /// [`WebSocketClient::unsubscribe_notifies`] first to take over.
    /// "Live" means the prior receiver has not been dropped: a stale
    /// slot whose receiver was dropped is treated as if no subscriber
    /// existed, and the new subscription installs silently. This
    /// matters because [`WebSocketClient`] is `Clone`; without the
    /// loud-replace contract, two holders of the same client could
    /// silently steal each other's subscription.
    ///
    /// Notifies that arrive while no subscriber is registered are
    /// dropped silently. High-rate notify use cases (e.g. server-pushed
    /// binary chunks) make any per-drop logging an avalanche, so we
    /// elide it; if the application needs to know whether messages were
    /// missed, it must subscribe before the first such notify.
    ///
    /// Memory hazard: the channel is unbounded. The transport read loop
    /// pushes every inbound notify into the channel without backpressure,
    /// so a slow or stalled consumer plus a high-rate producer will
    /// grow the buffer until the process OOMs. Application-level
    /// backpressure (e.g. ACK windows in a chunk protocol) is the right
    /// fix; the consumer must drain the receiver promptly. This API
    /// deliberately does not offer a bounded variant: dropping notifies
    /// on overflow corrupts chunk streams, and blocking the read loop
    /// on overflow stalls the request/response correlation path that
    /// shares the same socket.
    pub fn subscribe_notifies(
        &self,
    ) -> Result<mpsc::UnboundedReceiver<Message>, AlreadySubscribed> {
        let mut slot = self
            .inner
            .notify_tx
            .lock()
            .expect("repe websocket notify_tx mutex poisoned");
        if let Some(existing) = slot.as_ref() {
            if !existing.is_closed() {
                return Err(AlreadySubscribed);
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *slot = Some(tx);
        Ok(rx)
    }

    /// Drop the active notify subscription (if any). The next inbound
    /// notify will be dropped until [`Self::subscribe_notifies`] is
    /// called again.
    pub fn unsubscribe_notifies(&self) {
        *self
            .inner
            .notify_tx
            .lock()
            .expect("repe websocket notify_tx mutex poisoned") = None;
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

    pub async fn call_typed_beve_with_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Duration,
    ) -> Result<R, RepeError> {
        self.call_typed_beve_with_optional_timeout(path, body, Some(timeout_duration))
            .await
    }

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

    pub async fn registry_read<P: AsRef<str>>(&self, path: P) -> Result<Value, RepeError> {
        let resp = self.call_message(path).await?;
        resp.json_body::<Value>()
    }

    pub async fn registry_read_typed<P: AsRef<str>, R: DeserializeOwned>(
        &self,
        path: P,
    ) -> Result<R, RepeError> {
        let resp = self.call_message(path).await?;
        resp.json_body::<R>()
    }

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

    pub async fn registry_write_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json(path, body).await
    }

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

    pub async fn batch_json(
        &self,
        requests: Vec<(String, Value)>,
    ) -> Vec<Result<Value, RepeError>> {
        self.batch_json_inner(requests, None).await
    }

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
        let mut pending_guard = PendingRequestGuard::register(&self.inner, id, sender)?;

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

        let response = received?;
        pending_guard.disarm();
        Self::validate_response(id, response)
    }

    async fn write_request(&self, msg: &Message) -> Result<(), RepeError> {
        let mut writer = self.inner.writer.lock().await;
        writer
            .send(WsMessage::Binary(msg.to_vec()))
            .await
            .map_err(websocket_transport_error)?;
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
                    ErrorKind::InvalidData,
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

fn spawn_response_loop(mut reader: WsReader, inner: std::sync::Weak<WebSocketClientInner>) {
    tokio::spawn(async move {
        loop {
            let frame = match reader.next().await {
                Some(Ok(frame)) => frame,
                Some(Err(err)) => {
                    fail_all_pending(&inner, websocket_transport_error(err)).await;
                    break;
                }
                None => {
                    fail_all_pending(&inner, websocket_closed_error()).await;
                    break;
                }
            };

            let response = match decode_websocket_frame(frame) {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(err) => {
                    fail_all_pending(&inner, err).await;
                    break;
                }
            };

            let dispatch = {
                let Some(inner_ref) = inner.upgrade() else {
                    break;
                };

                if response.header.notify != 0 {
                    let notify_tx = inner_ref
                        .notify_tx
                        .lock()
                        .expect("repe websocket notify_tx mutex poisoned")
                        .clone();
                    match notify_tx {
                        Some(sender) => PendingDispatch::Notify { sender, response },
                        None => PendingDispatch::DroppedNotify,
                    }
                } else {
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
                }
            };

            match dispatch {
                PendingDispatch::Matched { sender, response } => {
                    let _ = sender.send(Ok(response));
                }
                PendingDispatch::Notify { sender, response } => {
                    // Receiver dropped: clear the slot only if it still
                    // points at the same channel we just failed to send
                    // on. A concurrent `subscribe_notifies` call may
                    // have replaced the slot between our send attempt
                    // and the lock reacquire; clearing unconditionally
                    // would null out that fresh subscription.
                    if sender.send(response).is_err() {
                        if let Some(inner_ref) = inner.upgrade() {
                            let mut slot = inner_ref
                                .notify_tx
                                .lock()
                                .expect("repe websocket notify_tx mutex poisoned");
                            let stale = slot
                                .as_ref()
                                .map(|cur| cur.same_channel(&sender))
                                .unwrap_or(false);
                            if stale {
                                *slot = None;
                            }
                        }
                    }
                }
                PendingDispatch::DroppedNotify => {
                    // No subscriber; silent drop is intentional. Subscribers
                    // attach lazily, and chunk-style notifies fire at high
                    // rate; logging would drown stderr.
                }
                PendingDispatch::Unrecognized { got_id } => {
                    eprintln!(
                        "[repe] dropping websocket response for unrecognized request id {got_id}"
                    );
                }
            }
        }
    });
}

fn decode_websocket_frame(frame: WsMessage) -> Result<Option<Message>, RepeError> {
    match frame {
        WsMessage::Binary(payload) => Message::from_slice_exact(&payload).map(Some),
        WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => Ok(None),
        WsMessage::Close(_) => Err(websocket_closed_error()),
        WsMessage::Text(_) => Err(websocket_invalid_data_error(
            "websocket transport requires binary messages",
        )),
    }
}

async fn fail_all_pending(inner: &std::sync::Weak<WebSocketClientInner>, err: RepeError) {
    let Some(inner_ref) = inner.upgrade() else {
        return;
    };

    let _ = close_writer(&inner_ref).await;

    let waiters = {
        let mut pending = lock_pending_map(&inner_ref.pending);
        pending.drain().collect::<Vec<_>>()
    };

    for (request_id, sender) in waiters {
        let _ = sender.send(Err(clone_fatal_error_for_waiter(&err, request_id)));
    }
}

async fn close_writer(inner: &Arc<WebSocketClientInner>) -> Result<(), RepeError> {
    let mut writer = inner.writer.lock().await;
    let _ = writer.send(WsMessage::Close(None)).await;
    writer.close().await.map_err(websocket_transport_error)
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

fn duplicate_request_id_error(request_id: u64) -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::AlreadyExists,
        format!("request id {request_id} is already pending"),
    ))
}

fn batch_worker_join_error(err: JoinError) -> RepeError {
    RepeError::Io(std::io::Error::other(format!("batch worker failed: {err}")))
}

fn batch_worker_missing_result_error() -> RepeError {
    RepeError::Io(std::io::Error::other("batch worker missing result"))
}

fn websocket_connect_error(err: tungstenite::Error) -> std::io::Error {
    match err {
        tungstenite::Error::Io(io_err) => io_err,
        other => std::io::Error::other(other.to_string()),
    }
}

fn websocket_transport_error(err: tungstenite::Error) -> RepeError {
    match err {
        tungstenite::Error::Io(io_err) => RepeError::Io(io_err),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => RepeError::Io(
            std::io::Error::new(ErrorKind::ConnectionAborted, "websocket connection closed"),
        ),
        other => RepeError::Io(std::io::Error::other(other.to_string())),
    }
}

fn websocket_closed_error() -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::ConnectionAborted,
        "websocket connection closed",
    ))
}

fn websocket_invalid_data_error(message: &str) -> RepeError {
    RepeError::Io(std::io::Error::new(ErrorKind::InvalidData, message))
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
    use crate::constants::QueryFormat;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test(flavor = "current_thread")]
    async fn websocket_client_rejects_trailing_bytes_in_binary_frame() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let request = ws.next().await.unwrap().unwrap();
            let request = match request {
                WsMessage::Binary(payload) => Message::from_slice_exact(&payload).unwrap(),
                other => panic!("unexpected frame: {other:?}"),
            };

            let mut response = Message::builder()
                .id(request.header.id)
                .query_str("/bad")
                .query_format(QueryFormat::JsonPointer)
                .body_json(&serde_json::json!({ "ok": true }))
                .unwrap()
                .build()
                .to_vec();
            response.extend_from_slice(&[0xAA, 0xBB]);

            ws.send(WsMessage::Binary(response)).await.unwrap();
        });

        let client = WebSocketClient::connect(&format!("ws://{addr}/bad"))
            .await
            .unwrap();
        let err = client
            .call_json("/bad", &serde_json::json!({}))
            .await
            .unwrap_err();

        match err {
            RepeError::LengthMismatch { .. } => {}
            other => panic!("unexpected error: {other:?}"),
        }

        server_task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscribe_notifies_routes_inbound_notify_to_subscriber() {
        // Driven by a request/response round-trip so the test stays
        // deterministic regardless of the tokio runtime flavor: the
        // server pushes the notify only after seeing the client's
        // request, by which point the subscription is already
        // installed.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let request = match ws.next().await.unwrap().unwrap() {
                WsMessage::Binary(payload) => Message::from_slice_exact(&payload).unwrap(),
                other => panic!("unexpected frame: {other:?}"),
            };

            // Send the notify before the response so the response loop
            // sees it first; the client will recv() it after the
            // request future completes.
            let notify = Message::builder()
                .id(0)
                .notify(true)
                .query_str("/push")
                .query_format(QueryFormat::JsonPointer)
                .body_json(&serde_json::json!({ "kind": "hello" }))
                .unwrap()
                .build();
            ws.send(WsMessage::Binary(notify.to_vec())).await.unwrap();

            let response = Message::builder()
                .id(request.header.id)
                .query_str("/echo")
                .query_format(QueryFormat::JsonPointer)
                .body_json(&serde_json::json!({ "ok": true }))
                .unwrap()
                .build();
            ws.send(WsMessage::Binary(response.to_vec())).await.unwrap();
        });

        let client = WebSocketClient::connect(&format!("ws://{addr}/notify"))
            .await
            .unwrap();
        let mut notifies = client.subscribe_notifies().expect("first subscribe");

        let resp = client
            .call_json("/echo", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(resp, serde_json::json!({ "ok": true }));

        let pushed = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .expect("notify did not arrive in time")
            .expect("subscriber channel closed");
        assert!(pushed.header.notify != 0);
        assert_eq!(pushed.query_str().unwrap(), "/push");
        let body: serde_json::Value = pushed.json_body().unwrap();
        assert_eq!(body, serde_json::json!({ "kind": "hello" }));

        drop(client);
        server_task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscribe_notifies_rejects_double_subscribe() {
        // Bind a listener but never fully serve - the test exercises
        // only the local `subscribe_notifies` slot management.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            let _ws = accept_async(_stream).await.unwrap();
            // Hold the connection open so the client doesn't see EOF
            // mid-test.
            std::future::pending::<()>().await;
        });

        let client = WebSocketClient::connect(&format!("ws://{addr}/double"))
            .await
            .unwrap();

        let first = client.subscribe_notifies().expect("first subscribe");
        let err = client
            .subscribe_notifies()
            .expect_err("second subscribe should fail");
        assert_eq!(err, AlreadySubscribed);

        // Cloning the client doesn't sneak past the loud-replace
        // contract either: the slot lives on `inner`, which clones
        // share.
        let cloned = client.clone();
        let err2 = cloned
            .subscribe_notifies()
            .expect_err("clone subscribe should fail too");
        assert_eq!(err2, AlreadySubscribed);

        // Drop the live receiver. The stale slot now lets a new
        // subscription replace silently.
        drop(first);
        // Spin briefly so tokio observes the receiver drop on the
        // sender's side (`is_closed()` flips synchronously, but be
        // explicit about ordering).
        tokio::task::yield_now().await;
        let _second = client
            .subscribe_notifies()
            .expect("subscribe after drop should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unsubscribe_notifies_clears_active_subscription() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            let _ws = accept_async(_stream).await.unwrap();
            std::future::pending::<()>().await;
        });

        let client = WebSocketClient::connect(&format!("ws://{addr}/unsub"))
            .await
            .unwrap();

        let first = client.subscribe_notifies().expect("first subscribe");
        // Without unsubscribe, the second call must fail.
        assert_eq!(client.subscribe_notifies().unwrap_err(), AlreadySubscribed);

        // After unsubscribe, the slot is empty even though the prior
        // receiver is still alive: subscribing yields a fresh receiver,
        // and the original receiver is no longer wired up.
        client.unsubscribe_notifies();
        let _second = client
            .subscribe_notifies()
            .expect("subscribe after unsubscribe should succeed");
        // Holding `first` here ensures we're not just relying on the
        // stale-slot path (which only kicks in after the receiver is
        // dropped).
        drop(first);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notifies_arriving_before_subscribe_are_dropped() {
        // Two request/response round-trips bracket the notifies, so
        // ordering relies on REPE message order on the wire rather
        // than on sleeps:
        //   1) /dropped notify -> /flush req -> /flush resp
        //      (when /flush returns, the response loop has dispatched
        //       and dropped /dropped because no subscriber existed)
        //   2) subscribe -> /ready req -> /ready resp -> /observed notify
        //      (server only emits /observed after seeing /ready, so it
        //       can't race ahead of subscribe)
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let dropped = Message::builder()
                .id(0)
                .notify(true)
                .query_str("/dropped")
                .query_format(QueryFormat::JsonPointer)
                .body_json(&serde_json::json!({}))
                .unwrap()
                .build();
            ws.send(WsMessage::Binary(dropped.to_vec())).await.unwrap();

            for _ in 0..2 {
                let request = match ws.next().await.unwrap().unwrap() {
                    WsMessage::Binary(payload) => Message::from_slice_exact(&payload).unwrap(),
                    other => panic!("unexpected frame: {other:?}"),
                };
                let response = Message::builder()
                    .id(request.header.id)
                    .query_str("/ack")
                    .query_format(QueryFormat::JsonPointer)
                    .body_json(&serde_json::json!({ "ok": true }))
                    .unwrap()
                    .build();
                ws.send(WsMessage::Binary(response.to_vec())).await.unwrap();
            }

            let observed = Message::builder()
                .id(0)
                .notify(true)
                .query_str("/observed")
                .query_format(QueryFormat::JsonPointer)
                .body_json(&serde_json::json!({}))
                .unwrap()
                .build();
            ws.send(WsMessage::Binary(observed.to_vec())).await.unwrap();
        });

        let client = WebSocketClient::connect(&format!("ws://{addr}/notify"))
            .await
            .unwrap();

        // First round-trip: when this returns, the response loop has
        // processed every frame the server sent before the response,
        // including /dropped.
        client
            .call_json("/flush", &serde_json::json!({}))
            .await
            .unwrap();

        let mut notifies = client.subscribe_notifies().expect("first subscribe");

        // /dropped was processed (and dropped) before we subscribed,
        // so try_recv must be empty at this instant.
        match notifies.try_recv() {
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            other => panic!("expected empty receiver, got {other:?}"),
        }

        // Second round-trip: tells the server we've subscribed. The
        // server pushes /observed only after seeing this request, so
        // /observed cannot reach the response loop before we
        // subscribed.
        client
            .call_json("/ready", &serde_json::json!({}))
            .await
            .unwrap();

        let next = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .expect("expected later notify")
            .expect("channel closed");
        assert_eq!(next.query_str().unwrap(), "/observed");

        drop(client);
        server_task.await.unwrap();
    }
}
