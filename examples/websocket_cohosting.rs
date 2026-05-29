//! One-port co-hosting: REPE-over-WebSocket and plain HTTP on one port.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example websocket_cohosting --features websocket
//! ```
//!
//! A service often wants a REPE WebSocket endpoint *and* a few sibling
//! plain-HTTP routes (a `/healthz` liveness probe, a small JSON config
//! read) on the **same** TCP port, without pulling in a second HTTP
//! framework. repe makes that a peek-then-fork: own the accept loop,
//! classify each accepted stream, send WebSocket upgrades to REPE and
//! everything else to a tiny HTTP handler.
//!
//! This fills in the two pieces the `docs/websocket.md` co-hosting
//! sketch leaves as placeholders:
//!
//! * [`repe::is_websocket_upgrade`] — the non-destructive WS-vs-HTTP
//!   classifier (it peeks, leaving the request bytes intact for the
//!   subsequent [`WebSocketServer::accept`]), and
//! * `serve_http` below — a minimal, dependency-free HTTP/1.1 handler for
//!   the sibling routes.
//!
//! The program runs the co-hosting server and then drives both forks
//! against it in one process: a [`WebSocketClient`] round-trips a REPE
//! call, and a raw `GET /healthz` (written over a plain TCP socket) gets
//! an HTTP `200`.

use repe::server::Router;
use repe::{WebSocketClient, WebSocketServer, is_websocket_upgrade};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const REPE_PATH: &str = "/repe";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = Router::new().with_json("/ping", |_| Ok(json!({ "pong": true })));
    let shared = WebSocketServer::new(router).into_shared();

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;

    // The co-hosting accept loop: one TCP port, two protocols.
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _peer_addr)) = listener.accept().await else {
                break;
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                // Peek the request head without consuming it, then fork.
                match is_websocket_upgrade(&stream).await {
                    Ok(true) => {
                        // A WebSocket upgrade: hand the *original* stream to
                        // `accept`, which replays the handshake from the
                        // bytes the peek left in place.
                        if let Ok(ws) = WebSocketServer::accept(stream, REPE_PATH).await {
                            let _ = shared.serve_connection(ws).await;
                        }
                    }
                    Ok(false) => {
                        // Anything else is a plain HTTP request.
                        if let Err(err) = serve_http(stream).await {
                            eprintln!("[http] {err}");
                        }
                    }
                    Err(err) => eprintln!("[accept] peek failed: {err}"),
                }
            });
        }
    });

    // --- Fork 1: the REPE WebSocket endpoint. ---
    let client = WebSocketClient::connect(&format!("ws://{addr}{REPE_PATH}")).await?;
    let pong = client.call_json("/ping", &json!({})).await?;
    println!("[ws] /ping -> {pong}");
    assert_eq!(pong["pong"], true);
    drop(client);

    // --- Fork 2: a plain HTTP route on the same port. ---
    let status = http_get(addr, "/healthz").await?;
    println!("[http] GET /healthz -> {status}");
    assert!(status.starts_with("HTTP/1.1 200"));

    let body = http_get(addr, "/config").await?;
    println!("[http] GET /config -> {} bytes", body.len());
    assert!(body.contains("\"version\""));

    let missing = http_get(addr, "/nope").await?;
    assert!(missing.starts_with("HTTP/1.1 404"));

    println!("[main] both forks answered on one TCP port");
    server.abort();
    Ok(())
}

/// A minimal, dependency-free HTTP/1.1 handler for the sibling routes a
/// co-hosting embedder wants beside its WebSocket endpoint. It reads one
/// request, routes on the method + path, writes one response, and closes
/// the connection (`Connection: close` — no keep-alive).
///
/// This is deliberately small: just enough to answer a liveness probe and
/// a couple of JSON `GET`s. A production embedder would expand the routing
/// table (or reach for a real HTTP server) but would keep this exact
/// peek-then-fork shape on the accept loop.
async fn serve_http(mut stream: TcpStream) -> std::io::Result<()> {
    let Some(head) = read_request_head(&mut stream).await? else {
        // No request head (peer closed before sending one); nothing to do.
        return Ok(());
    };

    // The request line is the first line of the head:
    // "METHOD <target> HTTP/1.1" -> (method, path-without-query).
    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let path = target.split(['?', '#']).next().unwrap_or(target);

    match (method, path) {
        ("GET", "/healthz") => write_response(&mut stream, 200, "OK", "text/plain", b"ok").await,
        ("GET", "/config") => {
            let body = json!({ "version": env!("CARGO_PKG_VERSION"), "transport": "repe" });
            let bytes = serde_json::to_vec(&body).expect("serialize config");
            write_response(&mut stream, 200, "OK", "application/json", &bytes).await
        }
        ("GET", _) => {
            write_response(&mut stream, 404, "Not Found", "text/plain", b"not found").await
        }
        _ => {
            write_response(
                &mut stream,
                405,
                "Method Not Allowed",
                "text/plain",
                b"method not allowed",
            )
            .await
        }
    }
}

/// Read the full HTTP request head (up to and including the `\r\n\r\n`
/// header terminator), bounded so a misbehaving client cannot make us
/// buffer without limit. Returns `None` if the connection closed before a
/// head arrived.
///
/// Draining the whole head (not just the request line) matters: closing
/// the socket while bytes the peer already sent sit unread in our receive
/// buffer makes the OS send a TCP `RST` instead of a clean `FIN`, which
/// the client sees as "connection reset" mid-read. The example serves only
/// `GET`s (no request body), so end-of-headers is the end of the request.
async fn read_request_head(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // EOF: return whatever we have (empty -> None).
            return Ok((!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned()));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        if buf.len() >= 8192 {
            // Header block too long; stop reading and treat as bad input
            // rather than buffering unbounded.
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}

/// Write a complete HTTP/1.1 response with a fixed `Content-Length` and
/// `Connection: close`.
async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        len = body.len(),
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

/// Tiny raw-socket HTTP client used only to exercise the plain-HTTP fork
/// in this example (so it needs no extra dependency). Returns the full
/// response (status line, headers, body) as a string.
async fn http_get(addr: std::net::SocketAddr, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(addr).await?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}
