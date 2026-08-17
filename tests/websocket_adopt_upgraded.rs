#![cfg(feature = "websocket")]
//! Serving a connection whose upgrade this crate did not perform.
//!
//! The shipped one-port recipe (`is_websocket_upgrade` + `WebSocketServer::accept`)
//! requires repe to own the `TcpStream` before any HTTP is parsed, so it cannot
//! serve an embedder whose routes live in an HTTP framework: that framework owns
//! the socket and answers the 101 itself. These tests cover the other direction —
//! the framework hands back an already-upgraded byte stream and repe adopts it.
//!
//! No HTTP framework is a dependency here, so the "framework" is hand-rolled:
//! the 101 is written by the test, exactly as `axum`/`hyper` would write it, and
//! the request is reassembled into an `http::Request` so the handshake-keyed path
//! is exercised through the same public API an embedder would use. That keeps
//! these tests an executable spec of the recipe rather than a test of somebody
//! else's upgrade machinery.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use repe::server::Router;
use repe::tokio_tungstenite::tungstenite::http;
use repe::{
    HandshakeContext, Message, PeerRegistry, QueryFormat, SharedWebSocketServer, ShutdownToken,
    WebSocketClient, WebSocketLimits, WebSocketServer, derive_accept_key,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn echo_router() -> Router {
    Router::new().with_json("/echo", |payload: Value| Ok(json!({ "saw": payload })))
}

/// Read the upgrade request off `stream` and answer `101`, the way an HTTP
/// framework's WebSocket route does. Returns the stream positioned at the first
/// WebSocket frame, plus the request itself — which is what an embedder holds
/// and can hand to `HandshakeContext::from_http_request`.
///
/// Deliberately minimal: it trusts the client to be a well-formed REPE client,
/// because what is under test is the adoption seam, not request parsing.
async fn hand_rolled_upgrade(stream: TcpStream) -> std::io::Result<(TcpStream, http::Request<()>)> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Err(std::io::Error::other(
            "client closed before the request line",
        ));
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| std::io::Error::other("malformed request line"))?
        .to_owned();

    let mut builder = http::Request::builder().method("GET").uri(&target);
    let mut key = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(std::io::Error::other("client closed mid-handshake"));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("sec-websocket-key") {
            key = value.to_owned();
        }
        builder = builder.header(name, value);
    }
    let request = builder
        .body(())
        .map_err(|err| std::io::Error::other(format!("rebuilding the request: {err}")))?;

    // `BufReader` may have buffered past the header block. It has not here (a
    // REPE client sends no frame until the 101 arrives), so unwrapping back to
    // the raw stream cannot drop buffered frame bytes. A framework that *can*
    // buffer past the request hands those bytes back separately, which is what
    // `adopt_upgraded_partially_read` is for.
    assert!(
        reader.buffer().is_empty(),
        "client pipelined frames with its upgrade; this helper would lose them"
    );
    let mut stream = reader.into_inner();

    let accept = derive_accept_key(key.as_bytes());
    stream
        .write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    Ok((stream, request))
}

/// How the test's accept loop hands an upgraded connection to repe.
#[derive(Clone, Copy)]
enum Serve {
    /// `serve_connection` — no captured handshake, no shared cancellation.
    Plain,
    /// `serve_connection_with_cancel_and_handshake` — the full-fidelity path an
    /// embedder that keys peers off the upgrade and drains on shutdown uses.
    WithHandshakeAndCancel,
}

/// Bind an ephemeral port whose connections are upgraded by the test and then
/// adopted by `server`. Returns the address to point a client at, and the
/// `ShutdownToken` those connections were served under.
async fn spawn_adopting_server(
    server: WebSocketServer,
    mode: Serve,
) -> (std::net::SocketAddr, ShutdownToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let shared = server.into_shared();
    let shutdown = ShutdownToken::new();
    let loop_shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let (shared, shutdown): (SharedWebSocketServer, ShutdownToken) =
                (shared.clone(), loop_shutdown.clone());
            tokio::spawn(async move {
                let Ok((stream, request)) = hand_rolled_upgrade(stream).await else {
                    return;
                };
                let ws = shared.adopt_upgraded(stream).await;
                let _ = match mode {
                    Serve::Plain => shared.serve_connection(ws).await,
                    Serve::WithHandshakeAndCancel => {
                        let handshake = HandshakeContext::from_http_request(&request);
                        shared
                            .serve_connection_with_cancel_and_handshake(ws, handshake, &shutdown)
                            .await
                    }
                };
            });
        }
    });
    (addr, shutdown)
}

/// The headline case: a connection repe never handshook serves requests exactly
/// like one it accepted itself.
#[tokio::test]
async fn an_adopted_connection_serves_requests() {
    let (addr, _) = spawn_adopting_server(WebSocketServer::new(echo_router()), Serve::Plain).await;
    let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
        .await
        .expect("connect");

    let got: Value = client
        .call_typed_json::<_, _, Value>("/echo", &json!({ "n": 7 }))
        .await
        .expect("call");
    assert_eq!(got["saw"]["n"], 7);

    // Not a one-shot: the reader/writer pair keeps running on the adopted
    // stream the same as on an accepted one.
    let again: Value = client
        .call_typed_json::<_, _, Value>("/echo", &json!({ "n": 8 }))
        .await
        .expect("second call");
    assert_eq!(again["saw"]["n"], 8);
}

/// Push works on an adopted connection: connect hooks fire and an attached
/// `PeerRegistry` sees the peer, so a server-push consumer is not restricted to
/// connections repe accepted. This is the property that makes the seam useful
/// rather than merely present.
#[tokio::test]
async fn hooks_and_the_peer_registry_fire_on_an_adopted_connection() {
    let peers = PeerRegistry::new();
    let connects = Arc::new(AtomicUsize::new(0));
    let connects_hook = Arc::clone(&connects);

    let (addr, _) = spawn_adopting_server(
        WebSocketServer::new(echo_router())
            .with_peer_registry(peers.clone())
            .on_peer_connect(move |_peer| {
                connects_hook.fetch_add(1, Ordering::SeqCst);
            }),
        Serve::Plain,
    )
    .await;

    let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
        .await
        .expect("connect");
    // Round-trip first: the connect hook runs before traffic is processed, so a
    // completed call proves it has already fired without polling for it.
    let _: Value = client
        .call_typed_json::<_, _, Value>("/echo", &json!({}))
        .await
        .expect("call");

    assert_eq!(connects.load(Ordering::SeqCst), 1);
    assert_eq!(peers.len(), 1, "the adopted peer should be registered");

    let delivered = peers
        .broadcast_notify_json("/ping", &json!({ "hello": true }))
        .expect("encode notify");
    assert_eq!(delivered.len(), 1);
    assert!(
        delivered.values().all(Result::is_ok),
        "a notify should reach a peer on an adopted connection: {delivered:?}"
    );
}

/// Teardown is the half that only runs on the way out, and it runs from a `Drop`
/// guard, so it is worth asserting separately from the connect side: dropping
/// the client must fire `on_peer_disconnect` and evict the peer.
#[tokio::test]
async fn disconnect_teardown_runs_on_an_adopted_connection() {
    let peers = PeerRegistry::new();
    let disconnects = Arc::new(AtomicUsize::new(0));
    let disconnects_hook = Arc::clone(&disconnects);

    let (addr, _) = spawn_adopting_server(
        WebSocketServer::new(echo_router())
            .with_peer_registry(peers.clone())
            .on_peer_disconnect(move |_id| {
                disconnects_hook.fetch_add(1, Ordering::SeqCst);
            }),
        Serve::Plain,
    )
    .await;

    {
        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .expect("connect");
        let _: Value = client
            .call_typed_json::<_, _, Value>("/echo", &json!({}))
            .await
            .expect("call");
        assert_eq!(peers.len(), 1);
    }

    wait_for(|| disconnects.load(Ordering::SeqCst) == 1 && peers.is_empty()).await;
}

/// The handshake-keyed path, which is the reason `HandshakeContext` is publicly
/// constructible: an embedder holding the upgrade request captures it and repe
/// fires `on_peer_connect_with_handshake`, so the peer can be `alias`ed by an
/// identity that rode in the query string.
#[tokio::test]
async fn handshake_hooks_fire_when_the_embedder_captures_the_request() {
    let peers = PeerRegistry::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, Option<String>)>::new()));
    let seen_hook = Arc::clone(&seen);
    let alias_peers = peers.clone();

    let (addr, _) = spawn_adopting_server(
        WebSocketServer::new(echo_router())
            .with_peer_registry(peers.clone())
            .on_peer_connect_with_handshake(move |peer, hs| {
                seen_hook
                    .lock()
                    .expect("lock")
                    .push((hs.path().to_owned(), hs.query().map(str::to_owned)));
                if let Some(token) = hs.query().and_then(|q| q.strip_prefix("token=")) {
                    alias_peers.alias(peer.peer_id(), token);
                }
            }),
        Serve::WithHandshakeAndCancel,
    )
    .await;

    let client = WebSocketClient::connect(&format!("ws://{addr}/repe?token=abc123"))
        .await
        .expect("connect");
    let _: Value = client
        .call_typed_json::<_, _, Value>("/echo", &json!({}))
        .await
        .expect("call");

    let captured = seen.lock().expect("lock").clone();
    assert_eq!(
        captured,
        vec![("/repe".to_owned(), Some("token=abc123".to_owned()))],
        "the hook should see the path and query of the upgrade the embedder answered"
    );
    assert!(
        peers.get_by("abc123").is_some(),
        "the peer should be addressable by the key that rode in the handshake"
    );
}

/// `adopt_upgraded` exists so the server's configured *inbound* thresholds reach
/// a connection repe did not handshake — they are fixed when the stream is
/// constructed and cannot be set afterwards, so calling `from_raw_socket`
/// directly would silently substitute the transport defaults.
///
/// Asserted on the constructed stream rather than through behavior, because
/// nothing observable from a client distinguishes the two: the outbound guard
/// (the next test) is threaded separately from `config.limits` and would pass
/// either way. Dropping the config argument in `adopt_upgraded` leaves the rest
/// of the suite green; this is the test that fails.
#[tokio::test]
async fn adopt_upgraded_fixes_the_servers_configured_inbound_thresholds() {
    let limits = WebSocketLimits::default()
        .with_max_incoming_frame_size(Some(4096))
        .with_max_incoming_message_size(Some(8192));
    let shared = WebSocketServer::new(echo_router())
        .with_limits(limits)
        .into_shared();

    let (server_io, _client_io) = tokio::io::duplex(1024);
    let ws = shared.adopt_upgraded(server_io).await;
    assert_eq!(ws.get_config().max_frame_size, Some(4096));
    assert_eq!(ws.get_config().max_message_size, Some(8192));

    // Same for the partially-read sibling, which takes the identical path.
    let (server_io, _client_io) = tokio::io::duplex(1024);
    let ws = shared
        .adopt_upgraded_partially_read(server_io, Vec::new())
        .await;
    assert_eq!(ws.get_config().max_frame_size, Some(4096));
    assert_eq!(ws.get_config().max_message_size, Some(8192));
}

/// The outbound guard reaches an adopted connection too. This one *is*
/// observable from a client, because an oversized response is refused and
/// substituted rather than sent.
#[tokio::test]
async fn an_adopted_connection_enforces_the_outbound_guard() {
    let router = Router::new().with_json("/blob", |payload: Value| {
        let len = payload.get("len").and_then(Value::as_u64).unwrap_or(0) as usize;
        Ok(json!({ "data": "x".repeat(len) }))
    });
    let small = WebSocketLimits::default().with_assumed_peer_frame_limit(Some(4096));
    let (addr, _) = spawn_adopting_server(
        WebSocketServer::new(router).with_limits(small),
        Serve::Plain,
    )
    .await;

    let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
        .await
        .expect("connect");
    let err = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "len": 64 * 1024 }))
        .await
        .expect_err("the configured outbound guard must refuse this response");
    assert!(
        err.to_string().contains("assumed peer frame limit"),
        "the refusal should name the guard, got: {err}"
    );
}

/// Cancelling the shared token winds an adopted connection down, which is what
/// makes the documented drain story true for a framework-hosted server: its
/// connections run on tasks the framework's own graceful shutdown never awaits.
#[tokio::test]
async fn cancelling_the_shared_token_winds_down_an_adopted_connection() {
    let peers = PeerRegistry::new();
    let (addr, shutdown) = spawn_adopting_server(
        WebSocketServer::new(echo_router()).with_peer_registry(peers.clone()),
        Serve::WithHandshakeAndCancel,
    )
    .await;

    let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
        .await
        .expect("connect");
    let _: Value = client
        .call_typed_json::<_, _, Value>("/echo", &json!({}))
        .await
        .expect("call");
    assert_eq!(peers.len(), 1);

    shutdown.cancel();

    // The reader stops on the cancelled token and teardown runs, evicting the
    // peer without the client having disconnected.
    wait_for(|| peers.is_empty()).await;
}

/// The API change itself: the serving path is generic over the byte stream, so
/// a connection that is not a `TcpStream` at all can be served. An in-memory
/// duplex stands in for the `hyper::upgrade::Upgraded` an embedder actually has
/// — neither is a socket this crate accepted, which is the whole point.
#[tokio::test]
async fn a_connection_that_is_not_a_tcp_stream_can_be_served() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let shared = WebSocketServer::new(echo_router()).into_shared();
    let ws = shared.adopt_upgraded(server_io).await;
    tokio::spawn(async move { shared.serve_connection(ws).await });

    let mut client = raw_client(client_io).await;
    assert_eq!(round_trip(&mut client, 9).await["saw"]["n"], 9);
}

/// Bytes a framework read past the upgrade request are decoded ahead of the
/// stream, so a client that pipelines its first frame with the handshake does
/// not lose it. The plain `adopt_upgraded` has nowhere to put them.
#[tokio::test]
async fn buffered_bytes_are_decoded_before_the_stream() {
    use futures_util::StreamExt;
    use repe::tokio_tungstenite::WebSocketStream;
    use repe::tokio_tungstenite::tungstenite::Message as WsMessage;
    use repe::tokio_tungstenite::tungstenite::protocol::Role;

    // Frame a request the way a client would, then hand those bytes to the
    // server as "already read past the request" rather than writing them.
    let (encoder_io, mut sink) = tokio::io::duplex(64 * 1024);
    let mut encoder = WebSocketStream::from_raw_socket(encoder_io, Role::Client, None).await;
    {
        use futures_util::SinkExt;
        encoder
            .send(WsMessage::Binary(request_bytes(11)))
            .await
            .expect("frame the request");
    }
    let pipelined = {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 4096];
        let n = sink.read(&mut buf).await.expect("read framed bytes");
        buf.truncate(n);
        buf
    };
    assert!(!pipelined.is_empty());

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let shared = WebSocketServer::new(echo_router()).into_shared();
    let ws = shared
        .adopt_upgraded_partially_read(server_io, pipelined)
        .await;
    tokio::spawn(async move { shared.serve_connection(ws).await });

    // The client never sent that request over the socket, yet the response
    // arrives — the buffered bytes were decoded.
    let mut client = raw_client(client_io).await;
    let frame = client
        .next()
        .await
        .expect("a response frame")
        .expect("frame is not an error");
    assert_eq!(decode(frame)["saw"]["n"], 11);
}

// ---- helpers for the handshake-free (raw stream) tests ----

type RawClient = repe::tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>;

/// A client on a stream whose handshake was skipped, pairing with the server
/// side's `adopt_upgraded`.
async fn raw_client(io: tokio::io::DuplexStream) -> RawClient {
    use repe::tokio_tungstenite::WebSocketStream;
    use repe::tokio_tungstenite::tungstenite::protocol::Role;
    WebSocketStream::from_raw_socket(io, Role::Client, None).await
}

/// The wire bytes of an `/echo` request carrying `{"n": n}`.
fn request_bytes(n: u64) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_format(QueryFormat::JsonPointer)
        .query_str("/echo")
        .body_json(&json!({ "n": n }))
        .expect("body")
        .build()
        .into_wire_bytes()
}

fn decode(frame: repe::tokio_tungstenite::tungstenite::Message) -> Value {
    use repe::tokio_tungstenite::tungstenite::Message as WsMessage;
    let WsMessage::Binary(bytes) = frame else {
        panic!("expected a binary REPE frame, got {frame:?}");
    };
    Message::from_slice_exact(&bytes)
        .expect("decode response")
        .json_body()
        .expect("json body")
}

async fn round_trip(client: &mut RawClient, n: u64) -> Value {
    use futures_util::{SinkExt, StreamExt};
    use repe::tokio_tungstenite::tungstenite::Message as WsMessage;
    client
        .send(WsMessage::Binary(request_bytes(n)))
        .await
        .expect("send");
    let frame = client
        .next()
        .await
        .expect("a response frame")
        .expect("frame is not an error");
    decode(frame)
}

/// Poll `cond` until it holds. Teardown runs on the connection's own task, so
/// there is no completion to await from here; the alternative is a fixed sleep,
/// which is both slower and flakier.
async fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition did not hold within 2s");
}
