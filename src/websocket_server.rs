use crate::async_client::AsyncClient;
use crate::constants::QueryFormat;
use crate::error::RepeError;
use crate::message::Message;
use crate::peer::{
    CallContext, NotifyBody, PeerHandle, PeerId, PeerRegistry, PeerSendError, PeerSink,
};
use crate::server::Router;
use crate::server_request::route_request_with_ctx;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::{self, Message as WsMessage, http::StatusCode};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

type ConnectHook = Arc<dyn Fn(PeerHandle) + Send + Sync>;
type DisconnectHook = Arc<dyn Fn(PeerId) + Send + Sync>;

/// Default per-connection outbound channel capacity.
///
/// Each accepted connection allocates a bounded `tokio::sync::mpsc`
/// channel of this size for outbound messages (responses + pushed
/// notifies). A full channel surfaces back as
/// [`PeerSendError::Full`](crate::PeerSendError) on the next
/// [`PeerHandle::send_notify`] attempt. Override per server with
/// [`WebSocketServer::with_outbound_capacity`].
pub const DEFAULT_OUTBOUND_CAPACITY: usize = 256;

/// Upper bound on how long the writer task spends draining queued
/// messages after the connection's reader has exited. A slow or
/// unresponsive peer (where TCP has not yet errored) cannot pin the
/// connection task open past this deadline.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub struct WebSocketServer {
    router: Router,
    outbound_capacity: usize,
    peer_id_counter: Arc<AtomicU64>,
    on_connect: Vec<ConnectHook>,
    on_disconnect: Vec<DisconnectHook>,
}

impl WebSocketServer {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            outbound_capacity: DEFAULT_OUTBOUND_CAPACITY,
            peer_id_counter: Arc::new(AtomicU64::new(0)),
            on_connect: Vec::new(),
            on_disconnect: Vec::new(),
        }
    }

    pub async fn listen<A: ToSocketAddrs>(addr: A) -> std::io::Result<TcpListener> {
        TcpListener::bind(addr).await
    }

    /// Per-connection outbound channel capacity. Defaults to
    /// [`DEFAULT_OUTBOUND_CAPACITY`]. A full channel returns
    /// [`PeerSendError::Full`](crate::PeerSendError) from
    /// [`PeerHandle::send_notify`]; the embedder decides whether to
    /// retry, drop, or prune.
    ///
    /// Panics if `capacity == 0` (bounded mpsc with zero capacity has
    /// surprising semantics; this is programmer error).
    pub fn with_outbound_capacity(mut self, capacity: usize) -> Self {
        assert!(
            capacity >= 1,
            "WebSocketServer outbound capacity must be >= 1"
        );
        self.outbound_capacity = capacity;
        self
    }

    /// Attach a [`PeerRegistry`] so this server's accepted peers are
    /// inserted on connect and removed on disconnect. The registry is
    /// `Arc`-shared so background tasks can clone it and broadcast.
    ///
    /// Sugar over [`on_peer_connect`](Self::on_peer_connect) +
    /// [`on_peer_disconnect`](Self::on_peer_disconnect). Calling this
    /// does not exclude additional connect/disconnect callbacks; hooks
    /// fire in registration order.
    ///
    /// The server's `PeerId` allocator is swapped for the registry's,
    /// so two `WebSocketServer`s sharing one registry mint
    /// non-colliding ids. If you call `with_peer_registry` more than
    /// once on the same server, the most recent registry's counter
    /// wins.
    pub fn with_peer_registry(mut self, registry: PeerRegistry) -> Self {
        self.peer_id_counter = registry.id_counter();
        let insert_registry = registry.clone();
        let remove_registry = registry;
        self.on_peer_connect(move |peer| insert_registry.insert(peer))
            .on_peer_disconnect(move |id| {
                remove_registry.remove(id);
            })
    }

    /// Run `f` from the per-connection task on accept, just after the
    /// `PeerHandle` is built and *before* the reader/writer tasks
    /// start processing traffic. Notifies queued from `f` via
    /// `peer.send_notify(...)` are guaranteed to land on the wire
    /// before any response.
    ///
    /// The callback runs synchronously on the accept task and must not
    /// block; offload work to a channel or `tokio::spawn` if needed.
    pub fn on_peer_connect<F>(mut self, f: F) -> Self
    where
        F: Fn(PeerHandle) + Send + Sync + 'static,
    {
        self.on_connect.push(Arc::new(f));
        self
    }

    /// Run `f` when the connection closes (any exit path: clean Close
    /// frame, transport error, handler panic). Fires exactly once per
    /// accepted connection.
    pub fn on_peer_disconnect<F>(mut self, f: F) -> Self
    where
        F: Fn(PeerId) + Send + Sync + 'static,
    {
        self.on_disconnect.push(Arc::new(f));
        self
    }

    pub async fn serve<A: ToSocketAddrs>(self, addr: A, path: &str) -> std::io::Result<()> {
        let listener = Self::listen(addr).await?;
        self.serve_listener(listener, path).await
    }

    pub async fn serve_listener(self, listener: TcpListener, path: &str) -> std::io::Result<()> {
        let expected_path = normalize_path(path);
        let config = Arc::new(ConnectionConfig {
            router: self.router,
            outbound_capacity: self.outbound_capacity,
            peer_id_counter: self.peer_id_counter,
            on_connect: Arc::new(self.on_connect),
            on_disconnect: Arc::new(self.on_disconnect),
        });

        loop {
            let (stream, _addr) = listener.accept().await?;
            let expected_path = expected_path.clone();
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                match accept_repe_websocket(stream, &expected_path).await {
                    Ok(ws_stream) => {
                        if let Err(err) = handle_connection_with_config(ws_stream, config).await {
                            eprintln!("[repe] websocket connection error: {err}");
                        }
                    }
                    Err(err) => {
                        eprintln!("[repe] websocket handshake error: {err}");
                    }
                }
            });
        }
    }
}

struct ConnectionConfig {
    router: Router,
    outbound_capacity: usize,
    peer_id_counter: Arc<AtomicU64>,
    on_connect: Arc<Vec<ConnectHook>>,
    on_disconnect: Arc<Vec<DisconnectHook>>,
}

pub async fn proxy_connection<S>(
    ws_stream: WebSocketStream<S>,
    upstream: AsyncClient,
) -> Result<(), RepeError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = ws_stream.split();

    loop {
        let frame = match reader.next().await {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Err(websocket_transport_error(err)),
            None => break,
        };

        let request = match decode_request_frame(frame)? {
            FrameAction::Message(message) => message,
            FrameAction::Continue => continue,
            FrameAction::Close => break,
        };

        if let Some(response) = upstream.forward_message(&request).await? {
            writer
                .send(WsMessage::Binary(response.to_vec()))
                .await
                .map_err(websocket_transport_error)?;
        }
    }

    let _ = writer.send(WsMessage::Close(None)).await;
    writer.close().await.map_err(websocket_transport_error)
}

async fn accept_repe_websocket(
    stream: TcpStream,
    expected_path: &str,
) -> Result<WebSocketStream<TcpStream>, RepeError> {
    accept_hdr_async(
        stream,
        WebSocketPathValidator {
            expected: expected_path.to_owned(),
        },
    )
    .await
    .map_err(websocket_transport_error)
}

async fn handle_connection_with_config(
    ws_stream: WebSocketStream<TcpStream>,
    config: Arc<ConnectionConfig>,
) -> Result<(), RepeError> {
    let (outbound_tx, outbound_rx) = mpsc::channel::<Message>(config.outbound_capacity);

    let peer_id_value = config.peer_id_counter.fetch_add(1, Ordering::Relaxed);
    debug_assert!(
        peer_id_value != PeerId::DETACHED.0,
        "peer id counter collided with PeerId::DETACHED sentinel"
    );
    let peer_id = PeerId(peer_id_value);

    let sink = Arc::new(WsPeerSink {
        tx: outbound_tx.clone(),
    });
    let peer = PeerHandle::new(peer_id, sink);

    // Build the disconnect guard *before* invoking connect hooks. If a
    // later connect hook panics after an earlier one (e.g. the
    // registry-insert hook) has already side-effected, unwinding still
    // runs the guard's Drop and the disconnect hook tears the
    // partial state back down. Without this ordering, a panicking
    // connect hook leaves the peer wedged in the registry forever.
    let (ws_writer, ws_reader) = ws_stream.split();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let writer_handle = tokio::spawn(writer_task(ws_writer, outbound_rx, shutdown_rx));

    let reader_result = {
        let _guard = DisconnectGuard {
            peer_id,
            hooks: Arc::clone(&config.on_disconnect),
        };

        // Fire connect hooks *before* the reader/writer tasks process
        // traffic, so any notifies queued here are already in the
        // outbound channel when the writer task begins draining.
        for hook in config.on_connect.iter() {
            hook(peer.clone());
        }

        // Keep `peer` in the reader scope; it is threaded into each
        // request's `CallContext` so handlers can push notifies back
        // to the originator (via `Router::with_json_ctx` /
        // `with_typed_ctx` or a registry-backed `RegistryCallable`).
        reader_task(ws_reader, &config.router, outbound_tx, peer).await
        // _guard drops here (on every exit path, including unwind),
        // firing disconnect hooks. Any registry entry holding a
        // PeerHandle clone (and thus a sender clone) is released,
        // helping the writer task drain.
    };

    // Tell the writer to drain any queued messages and exit, regardless
    // of whether sender clones still linger in user-held PeerHandles.
    let _ = shutdown_tx.send(());

    let writer_result = match writer_handle.await {
        Ok(r) => r,
        Err(join_err) => Err(RepeError::Io(std::io::Error::other(format!(
            "websocket writer task panicked: {join_err}"
        )))),
    };

    reader_result.and(writer_result)
}

async fn reader_task(
    mut ws_reader: SplitStream<WebSocketStream<TcpStream>>,
    router: &Router,
    outbound_tx: mpsc::Sender<Message>,
    peer: PeerHandle,
) -> Result<(), RepeError> {
    loop {
        let frame = match ws_reader.next().await {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Err(websocket_transport_error(err)),
            None => break,
        };

        let request = match decode_request_frame(frame)? {
            FrameAction::Message(message) => message,
            FrameAction::Continue => continue,
            FrameAction::Close => break,
        };

        // Build a CallContext threading the calling peer to handlers.
        // Handlers registered via `Router::with_json_ctx` /
        // `with_typed_ctx` (or registry-backed callables that take a
        // `CallContext`) can reach `ctx.peer()` and push notifies back
        // through this connection's outbound channel during request
        // handling.
        let path = request.query_str().unwrap_or("");
        let ctx = CallContext::new(path, &peer);
        if let Some(response) = route_request_with_ctx(router, &request, &ctx) {
            if outbound_tx.send(response).await.is_err() {
                // Writer task exited (likely wire error); abandon
                // reader. The disconnect guard will still fire when
                // we return.
                break;
            }
        }
    }
    // `peer` drops here, releasing this connection's sender clone of
    // the outbound channel and helping the writer drain.
    drop(peer);
    Ok(())
}

async fn writer_task(
    mut ws_writer: SplitSink<WebSocketStream<TcpStream>, WsMessage>,
    mut outbound_rx: mpsc::Receiver<Message>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<(), RepeError> {
    let mut result: Result<(), RepeError> = Ok(());
    loop {
        tokio::select! {
            biased;
            // Drain queued messages preferentially so already-enqueued
            // notifies make the wire before shutdown.
            msg = outbound_rx.recv() => match msg {
                Some(m) => {
                    if let Err(err) = ws_writer
                        .send(WsMessage::Binary(m.to_vec()))
                        .await
                    {
                        result = Err(websocket_transport_error(err));
                        break;
                    }
                }
                None => break,
            },
            _ = &mut shutdown_rx => {
                // Shutdown requested: flush whatever is immediately
                // available, then exit. Bounded by SHUTDOWN_DRAIN_TIMEOUT
                // so a slow peer (TCP still responsive but throttled)
                // cannot pin the connection task open. Sends that
                // arrive after we break are discarded when outbound_rx
                // drops.
                let deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
                while let Ok(m) = outbound_rx.try_recv() {
                    let send_fut = ws_writer.send(WsMessage::Binary(m.to_vec()));
                    match tokio::time::timeout_at(deadline, send_fut).await {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            result = Err(websocket_transport_error(err));
                            break;
                        }
                        Err(_) => break,
                    }
                }
                break;
            }
        }
    }
    let deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
    let _ = tokio::time::timeout_at(deadline, ws_writer.send(WsMessage::Close(None))).await;
    let _ = tokio::time::timeout_at(deadline, ws_writer.close()).await;
    result
}

pub(crate) struct WsPeerSink {
    tx: mpsc::Sender<Message>,
}

impl PeerSink for WsPeerSink {
    fn send_notify(&self, method: &str, body: NotifyBody) -> Result<(), PeerSendError> {
        let body_format = body.body_format();
        let body_bytes = body.into_bytes();
        let msg = Message::builder()
            .id(0)
            .notify(true)
            .query_str(method)
            .query_format(QueryFormat::JsonPointer)
            .body_format_code(u16::from(body_format))
            .body_bytes(body_bytes)
            .build();

        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(PeerSendError::Full),
            Err(TrySendError::Closed(_)) => Err(PeerSendError::Disconnected),
        }
    }

    fn is_connected(&self) -> bool {
        !self.tx.is_closed()
    }
}

struct DisconnectGuard {
    peer_id: PeerId,
    hooks: Arc<Vec<DisconnectHook>>,
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        for hook in self.hooks.iter() {
            hook(self.peer_id);
        }
    }
}

enum FrameAction {
    Message(Message),
    Continue,
    Close,
}

fn decode_request_frame(frame: WsMessage) -> Result<FrameAction, RepeError> {
    match frame {
        WsMessage::Binary(payload) => Message::from_slice_exact(&payload).map(FrameAction::Message),
        WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => Ok(FrameAction::Continue),
        WsMessage::Close(_) => Ok(FrameAction::Close),
        WsMessage::Text(_) => Err(websocket_invalid_data_error(
            "websocket transport requires binary messages",
        )),
    }
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        path.trim_end_matches('/').to_string()
    } else {
        format!("/{}", path.trim_end_matches('/'))
    }
}

fn path_not_found_response(request: &Request) -> ErrorResponse {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Some(format!(
            "REPE websocket endpoint not found: {}",
            request.uri().path()
        )))
        .expect("valid handshake rejection response")
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

fn websocket_invalid_data_error(message: &str) -> RepeError {
    RepeError::Io(std::io::Error::new(ErrorKind::InvalidData, message))
}

struct WebSocketPathValidator {
    expected: String,
}

impl Callback for WebSocketPathValidator {
    #[allow(clippy::result_large_err)]
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        if request.uri().path() == self.expected {
            Ok(response)
        } else {
            Err(path_not_found_response(request))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::BodyFormat;
    use crate::websocket_client::WebSocketClient;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::Duration;

    async fn run_default_connection(
        ws_stream: WebSocketStream<TcpStream>,
        router: Router,
    ) -> Result<(), RepeError> {
        let config = Arc::new(ConnectionConfig {
            router,
            outbound_capacity: DEFAULT_OUTBOUND_CAPACITY,
            peer_id_counter: Arc::new(AtomicU64::new(0)),
            on_connect: Arc::new(Vec::new()),
            on_disconnect: Arc::new(Vec::new()),
        });
        handle_connection_with_config(ws_stream, config).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn websocket_server_roundtrip() {
        let router = Router::new().with_json("/mul", |value| {
            let a = value.get("a").and_then(|v| v.as_i64()).unwrap_or_default();
            let b = value.get("b").and_then(|v| v.as_i64()).unwrap_or_default();
            Ok(json!({ "prod": a * b }))
        });

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws_stream = accept_repe_websocket(stream, "/repe").await.unwrap();
            run_default_connection(ws_stream, router).await.unwrap();
        });

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let out = client
            .call_json("/mul", &json!({ "a": 6, "b": 7 }))
            .await
            .unwrap();
        assert_eq!(out["prod"], 42);

        drop(client);
        server_task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn websocket_proxy_forwards_raw_messages() {
        let upstream_router = Router::new().with_json("/echo", Ok);
        let upstream_listener = crate::async_server::AsyncServer::listen(("127.0.0.1", 0))
            .await
            .unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            crate::async_server::AsyncServer::new(upstream_router)
                .serve(upstream_listener)
                .await
                .unwrap();
        });

        let proxy_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let upstream = AsyncClient::connect(upstream_addr).await.unwrap();
            let (stream, _) = proxy_listener.accept().await.unwrap();
            let ws_stream = accept_repe_websocket(stream, "/repe").await.unwrap();
            proxy_connection(ws_stream, upstream).await.unwrap();
        });

        let client = WebSocketClient::connect(&format!("ws://{proxy_addr}/repe"))
            .await
            .unwrap();
        let out = client
            .call_json("/echo", &json!({ "ok": true }))
            .await
            .unwrap();
        assert_eq!(out, json!({ "ok": true }));

        drop(client);
        proxy_task.await.unwrap();
        upstream_task.abort();
    }

    async fn serve_one(
        listener: TcpListener,
        path: &str,
        server: WebSocketServer,
    ) -> tokio::task::JoinHandle<()> {
        let path = path.to_string();
        let config = Arc::new(ConnectionConfig {
            router: server.router,
            outbound_capacity: server.outbound_capacity,
            peer_id_counter: server.peer_id_counter,
            on_connect: Arc::new(server.on_connect),
            on_disconnect: Arc::new(server.on_disconnect),
        });
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let path = path.clone();
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    if let Ok(ws_stream) = accept_repe_websocket(stream, &path).await {
                        let _ = handle_connection_with_config(ws_stream, config).await;
                    }
                });
            }
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn registry_receives_broadcast() {
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let peers = PeerRegistry::new();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router).with_peer_registry(peers.clone());
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let mut notifies = client.subscribe_notifies().expect("subscribe");

        // Round-trip a request so we know the peer is registered by
        // the time the broadcast runs.
        let _ = client.call_json("/ping", &json!({})).await.unwrap();

        assert_eq!(peers.len(), 1);
        let results = peers
            .broadcast_notify_json("/announce", &json!({ "hello": "world" }))
            .unwrap();
        assert_eq!(results.len(), 1);
        for r in results.values() {
            r.as_ref().expect("broadcast send");
        }

        let pushed = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .expect("notify did not arrive")
            .expect("subscriber channel closed");
        assert!(pushed.header.notify != 0);
        assert_eq!(pushed.query_str().unwrap(), "/announce");
        let body: serde_json::Value = pushed.json_body().unwrap();
        assert_eq!(body, json!({ "hello": "world" }));

        drop(client);
        // Wait for disconnect to propagate.
        for _ in 0..50 {
            if peers.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(peers.is_empty(), "peer not removed after disconnect");
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_reaches_multiple_peers() {
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let peers = PeerRegistry::new();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router).with_peer_registry(peers.clone());
        let server_task = serve_one(listener, "/repe", server).await;

        let mut clients = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..3 {
            let c = WebSocketClient::connect(&format!("ws://{addr}/repe"))
                .await
                .unwrap();
            let rx = c.subscribe_notifies().expect("subscribe");
            // Synchronize so the peer is registered.
            c.call_json("/ping", &json!({})).await.unwrap();
            clients.push(c);
            receivers.push(rx);
        }

        assert_eq!(peers.len(), 3);
        let results = peers
            .broadcast_notify_json("/event", &json!({ "n": 1 }))
            .unwrap();
        assert_eq!(results.len(), 3);
        for r in results.values() {
            r.as_ref().expect("broadcast send");
        }

        for mut rx in receivers {
            let pushed = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("notify did not arrive")
                .expect("channel closed");
            assert_eq!(pushed.query_str().unwrap(), "/event");
        }

        for c in clients {
            drop(c);
        }
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connect_hook_queued_notify_arrives_before_response() {
        let router = Router::new().with_json("/sync", |_| Ok(json!({ "ok": true })));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router).on_peer_connect(|peer| {
            // Hook queues a notify before any traffic. The writer task
            // drains this before any response can be produced.
            let _ = peer.send_notify(
                "/welcome",
                NotifyBody::Json(serde_json::to_vec(&json!({ "id": peer.peer_id().0 })).unwrap()),
            );
        });
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let mut notifies = client.subscribe_notifies().expect("subscribe");

        // The welcome notify must arrive before the /sync response.
        let pushed = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .expect("welcome notify did not arrive")
            .expect("channel closed");
        assert_eq!(pushed.query_str().unwrap(), "/welcome");

        let resp = client.call_json("/sync", &json!({})).await.unwrap();
        assert_eq!(resp, json!({ "ok": true }));

        drop(client);
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disconnect_hook_fires_on_drop() {
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let disconnected = Arc::new(Mutex::new(Vec::<PeerId>::new()));
        let disconnected_clone = Arc::clone(&disconnected);

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router)
            .on_peer_disconnect(move |id| disconnected_clone.lock().unwrap().push(id));
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        // Synchronize so the connection is fully accepted.
        client.call_json("/ping", &json!({})).await.unwrap();
        drop(client);

        for _ in 0..50 {
            if !disconnected.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let observed = disconnected.lock().unwrap();
        assert_eq!(observed.len(), 1, "expected exactly one disconnect");
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_consumer_surfaces_backpressure() {
        // Capacity 1: the second outbound send saturates the channel.
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let peers = PeerRegistry::new();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router)
            .with_outbound_capacity(1)
            .with_peer_registry(peers.clone());
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        // Subscribe but never drain, so notifies accumulate in the
        // *server's* outbound channel as well (after the writer fills
        // its own buffer). The exact mechanism doesn't matter; we
        // just need to push enough that try_send eventually returns
        // Full.
        let _notifies = client.subscribe_notifies().expect("subscribe");
        client.call_json("/ping", &json!({})).await.unwrap();

        assert_eq!(peers.len(), 1);

        // Push notifies until we see Full. The bound is small so this
        // happens quickly; cap at a generous limit.
        let mut saw_full = false;
        for i in 0..1024 {
            let results = peers
                .broadcast_notify_json("/burst", &json!({ "i": i }))
                .unwrap();
            for (_, r) in results {
                match r {
                    Err(PeerSendError::Full) => {
                        saw_full = true;
                        break;
                    }
                    Err(PeerSendError::Disconnected) => break,
                    _ => {}
                }
            }
            if saw_full {
                break;
            }
        }
        assert!(saw_full, "expected PeerSendError::Full under backpressure");

        drop(client);
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_after_disconnect_surfaces_disconnected() {
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let peers = PeerRegistry::new();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router).with_peer_registry(peers.clone());
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        client.call_json("/ping", &json!({})).await.unwrap();
        assert_eq!(peers.len(), 1);

        // Snapshot the peer handles so we can keep them after the
        // registry removes them on disconnect.
        let captured = peers.peers();
        assert_eq!(captured.len(), 1);

        drop(client);
        for _ in 0..50 {
            if peers.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(peers.is_empty());

        // The captured handle's outbound channel is now closed.
        let err = captured[0]
            .send_notify("/after", NotifyBody::Json(b"{}".to_vec()))
            .unwrap_err();
        assert!(matches!(err, PeerSendError::Disconnected));

        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hooks_compose_in_registration_order() {
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let order = Arc::new(Mutex::new(Vec::<u32>::new()));
        let order_a = Arc::clone(&order);
        let order_b = Arc::clone(&order);

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router)
            .on_peer_connect(move |_| order_a.lock().unwrap().push(1))
            .on_peer_connect(move |_| order_b.lock().unwrap().push(2));
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        client.call_json("/ping", &json!({})).await.unwrap();

        let snapshot = order.lock().unwrap().clone();
        assert_eq!(snapshot, vec![1, 2]);

        drop(client);
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_utf8_and_raw() {
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let peers = PeerRegistry::new();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router).with_peer_registry(peers.clone());
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let mut notifies = client.subscribe_notifies().expect("subscribe");
        client.call_json("/ping", &json!({})).await.unwrap();

        let utf_results = peers.broadcast_notify_utf8("/msg", "hello");
        for (_, r) in utf_results {
            r.expect("utf8 broadcast send");
        }

        let raw_results =
            peers.broadcast_notify_raw("/bytes", BodyFormat::RawBinary, &[1u8, 2, 3, 4]);
        for (_, r) in raw_results {
            r.expect("raw broadcast send");
        }

        let m1 = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m1.query_str().unwrap(), "/msg");
        assert_eq!(m1.body, b"hello".to_vec());
        assert_eq!(m1.header.body_format, BodyFormat::Utf8 as u16);

        let m2 = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m2.query_str().unwrap(), "/bytes");
        assert_eq!(m2.body, vec![1u8, 2, 3, 4]);
        assert_eq!(m2.header.body_format, BodyFormat::RawBinary as u16);

        drop(client);
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    #[should_panic(expected = "WebSocketServer outbound capacity must be >= 1")]
    async fn outbound_capacity_zero_panics() {
        let router = Router::new();
        let _ = WebSocketServer::new(router).with_outbound_capacity(0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panicking_connect_hook_still_runs_disconnect_cleanup() {
        let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let peers = PeerRegistry::new();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        // with_peer_registry's insert hook runs first; the second hook
        // panics, simulating a faulty embedder callback. The disconnect
        // guard must still fire on unwind and remove the peer.
        let server = WebSocketServer::new(router)
            .with_peer_registry(peers.clone())
            .on_peer_connect(|_| panic!("boom"));
        let server_task = serve_one(listener, "/repe", server).await;

        // The client's handshake will likely succeed (the panic happens
        // post-handshake during hook execution), but the connection
        // will be torn down. We just need the registry to be empty
        // after the dust settles.
        let _ = WebSocketClient::connect(&format!("ws://{addr}/repe")).await;

        for _ in 0..100 {
            if peers.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            peers.is_empty(),
            "panicking connect hook leaked peer in registry"
        );
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_pushes_notify_to_calling_peer_via_with_json_ctx() {
        // A handler registered via with_json_ctx pushes a progress
        // notify back to the calling peer mid-request. Both the
        // notify and the response must arrive on the same connection.
        let router = Router::new().with_json_ctx("/work", |ctx, _params| {
            if let Some(peer) = ctx.peer() {
                let _ = peer.send_notify(
                    "/progress",
                    NotifyBody::Json(serde_json::to_vec(&json!({ "stage": "running" })).unwrap()),
                );
            }
            Ok(json!({ "done": true }))
        });

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router);
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let mut notifies = client.subscribe_notifies().expect("subscribe");

        let resp = client.call_json("/work", &json!({})).await.unwrap();
        assert_eq!(resp, json!({ "done": true }));

        let pushed = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .expect("progress notify did not arrive")
            .expect("channel closed");
        assert!(pushed.header.notify != 0);
        assert_eq!(pushed.query_str().unwrap(), "/progress");
        let body: serde_json::Value = pushed.json_body().unwrap();
        assert_eq!(body, json!({ "stage": "running" }));

        drop(client);
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ctx_handler_sees_no_peer_under_legacy_handle() {
        // When a context-aware handler is invoked via the legacy
        // HandlerErased::handle path (no peer threaded), ctx.peer()
        // is None. This guards the TCP-transport / direct-dispatch
        // fallback shape so a ctx-aware handler does not panic when
        // there is no peer.
        let router = Router::new().with_json_ctx("/probe", |ctx, _params| {
            Ok(json!({ "has_peer": ctx.peer().is_some() }))
        });
        let handler = router.get("/probe").expect("handler exists");

        let req = Message::builder()
            .id(1)
            .query_str("/probe")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&json!({}))
            .unwrap()
            .build();
        let resp = handler.handle(&req).unwrap();
        let body: serde_json::Value = resp.json_body().unwrap();
        assert_eq!(body, json!({ "has_peer": false }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn registry_callable_receives_peer_via_with_context() {
        // A Registry-backed handler wrapped in `WithContext` must see
        // the calling peer when the request comes in over the
        // WebSocket transport.
        use crate::Registry;
        use crate::registry::WithContext;

        let registry = Arc::new(Registry::new());
        registry
            .register_function(
                "/work",
                WithContext(|ctx: &CallContext, _params| {
                    if let Some(peer) = ctx.peer() {
                        let _ = peer.send_notify(
                            "/progress",
                            NotifyBody::Json(
                                serde_json::to_vec(&json!({ "stage": "registry" })).unwrap(),
                            ),
                        );
                    }
                    Ok(json!({ "done": true }))
                }),
            )
            .unwrap();

        let router = Router::new().with_registry("", Arc::clone(&registry));

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router);
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let mut notifies = client.subscribe_notifies().expect("subscribe");

        let resp = client.call_json("/work", &json!({})).await.unwrap();
        assert_eq!(resp, json!({ "done": true }));

        let pushed = tokio::time::timeout(Duration::from_secs(2), notifies.recv())
            .await
            .expect("registry handler did not push a notify")
            .expect("channel closed");
        assert_eq!(pushed.query_str().unwrap(), "/progress");

        drop(client);
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn middleware_preserves_ctx_across_pipeline() {
        // Middleware that calls next.run(req) without knowing about
        // CallContext must still thread ctx to the leaf handler.
        // This is what lets existing middleware compose with new
        // context-aware handlers.
        use crate::server::Next;
        let router = Router::new()
            .with_middleware(|req: &Message, next: Next<'_>| next.run(req))
            .with_json_ctx("/work", |ctx, _params| {
                Ok(json!({ "has_peer": ctx.peer().is_some() }))
            });

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::new(router);
        let server_task = serve_one(listener, "/repe", server).await;

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let resp = client.call_json("/work", &json!({})).await.unwrap();
        assert_eq!(resp, json!({ "has_peer": true }));

        drop(client);
        server_task.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_registry_across_two_servers_mints_unique_ids() {
        let router_a = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let router_b = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));
        let peers = PeerRegistry::new();

        let la = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let aa = la.local_addr().unwrap();
        let lb = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let ab = lb.local_addr().unwrap();

        let sa = WebSocketServer::new(router_a).with_peer_registry(peers.clone());
        let sb = WebSocketServer::new(router_b).with_peer_registry(peers.clone());
        let ta = serve_one(la, "/repe", sa).await;
        let tb = serve_one(lb, "/repe", sb).await;

        let c1 = WebSocketClient::connect(&format!("ws://{aa}/repe"))
            .await
            .unwrap();
        let c2 = WebSocketClient::connect(&format!("ws://{ab}/repe"))
            .await
            .unwrap();
        // Round-trips so both peers are registered.
        c1.call_json("/ping", &json!({})).await.unwrap();
        c2.call_json("/ping", &json!({})).await.unwrap();

        // Both peers must be present with distinct ids.
        assert_eq!(peers.len(), 2);
        let snapshot = peers.peers();
        let ids: std::collections::HashSet<_> = snapshot.iter().map(|p| p.peer_id().0).collect();
        assert_eq!(ids.len(), 2, "shared registry minted colliding PeerIds");

        drop(c1);
        drop(c2);
        ta.abort();
        tb.abort();
    }
}
