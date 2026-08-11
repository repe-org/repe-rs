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
//! the 101 is written by the test, exactly as `axum`/`hyper` would write it.
//! That keeps the test an executable spec of the recipe rather than a test of
//! somebody else's upgrade machinery.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use repe::server::Router;
use repe::{
    Message, PeerRegistry, QueryFormat, WebSocketClient, WebSocketLimits, WebSocketServer,
    derive_accept_key,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn echo_router() -> Router {
    Router::new().with_json("/echo", |payload: Value| Ok(json!({ "saw": payload })))
}

/// Read the upgrade request off `stream` and answer `101`, the way an HTTP
/// framework's WebSocket route does, returning the stream positioned at the
/// first WebSocket frame. Deliberately minimal: it trusts the client to be a
/// well-formed REPE client, because what is under test is the adoption seam,
/// not request parsing.
async fn hand_rolled_upgrade(stream: TcpStream) -> std::io::Result<TcpStream> {
    let mut reader = BufReader::new(stream);
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
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
            .map(|(_, value)| value.trim())
        {
            key = value.to_owned();
        }
    }

    // `BufReader` may have buffered past the header block. It has not here (a
    // REPE client sends no frame until the 101 arrives), so unwrapping back to
    // the raw stream cannot drop buffered frame bytes.
    assert!(
        reader.buffer().is_empty(),
        "client sent frames before the 101; the recipe would lose them"
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
    Ok(stream)
}

/// Bind an ephemeral port whose connections are upgraded by the test and then
/// adopted by `server`, and return the address to point a client at.
async fn spawn_adopting_server(server: WebSocketServer) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let shared = server.into_shared();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                let Ok(stream) = hand_rolled_upgrade(stream).await else {
                    return;
                };
                let ws = shared.adopt_upgraded(stream).await;
                let _ = shared.serve_connection(ws).await;
            });
        }
    });
    addr
}

/// The headline case: a connection repe never handshook serves requests exactly
/// like one it accepted itself.
#[tokio::test]
async fn an_adopted_connection_serves_requests() {
    let addr = spawn_adopting_server(WebSocketServer::new(echo_router())).await;
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

    let addr = spawn_adopting_server(
        WebSocketServer::new(echo_router())
            .with_peer_registry(peers.clone())
            .on_peer_connect(move |_peer| {
                connects_hook.fetch_add(1, Ordering::SeqCst);
            }),
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

/// `adopt_upgraded` exists so the server's configured limits reach a connection
/// it did not handshake. Calling `from_raw_socket` directly would silently use
/// the transport defaults, so this asserts the outbound guard — the half that is
/// repe's own on an already-upgraded stream — is the one that was configured.
#[tokio::test]
async fn an_adopted_connection_carries_the_servers_configured_limits() {
    let router = Router::new().with_json("/blob", |payload: Value| {
        let len = payload.get("len").and_then(Value::as_u64).unwrap_or(0) as usize;
        Ok(json!({ "data": "x".repeat(len) }))
    });
    let small = WebSocketLimits::default().with_assumed_peer_frame_limit(Some(4096));
    let addr = spawn_adopting_server(WebSocketServer::new(router).with_limits(small)).await;

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

/// The API change itself: the serving path is generic over the byte stream, so
/// a connection that is not a `TcpStream` at all can be served. An in-memory
/// duplex stands in for the `hyper::upgrade::Upgraded` an embedder actually has
/// — neither is a socket this crate accepted, which is the whole point.
#[tokio::test]
async fn a_connection_that_is_not_a_tcp_stream_can_be_served() {
    // Through repe's own re-export, which is how an embedder names these
    // types at the version repe is built against.
    use repe::tokio_tungstenite::WebSocketStream;
    use repe::tokio_tungstenite::tungstenite::Message as WsMessage;
    use repe::tokio_tungstenite::tungstenite::protocol::Role;

    use futures_util::{SinkExt, StreamExt};

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let shared = WebSocketServer::new(echo_router()).into_shared();
    let ws = shared.adopt_upgraded(server_io).await;
    tokio::spawn(async move { shared.serve_connection(ws).await });

    // The server side skipped the handshake, so the client must too; this is
    // the same `from_raw_socket` pairing, in the client role.
    let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;

    let request = Message::builder()
        .id(1)
        .query_format(QueryFormat::JsonPointer)
        .query_str("/echo")
        .body_json(&json!({ "n": 9 }))
        .expect("body")
        .build();
    client
        .send(WsMessage::Binary(request.into_wire_bytes()))
        .await
        .expect("send");

    let frame = client
        .next()
        .await
        .expect("a response frame")
        .expect("frame is not an error");
    let WsMessage::Binary(bytes) = frame else {
        panic!("expected a binary REPE frame, got {frame:?}");
    };
    let response = Message::from_slice_exact(&bytes).expect("decode response");
    let body: Value = response.json_body().expect("json body");
    assert_eq!(body["saw"]["n"], 9);
}
