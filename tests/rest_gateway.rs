//! End-to-end coverage of the REST gateway's HTTP layer.
//!
//! The mapping itself is unit-tested in `src/rest.rs` against `RestGateway::respond`,
//! with no socket. What is left to prove here is the part that only a real
//! connection exercises: that headers survive the trip, that a `HEAD` reports a
//! length while sending no body, that a `304` really carries no body, that an
//! oversized body is refused before it is buffered, and that the connection
//! stays usable afterwards.
//!
//! Requests are written as raw HTTP/1.1 rather than through a client crate: the
//! gateway's contract is with the wire, and asserting against bytes we wrote
//! ourselves keeps the test from inheriting a client library's opinions about
//! what a response should look like.

#![cfg(feature = "rest")]

use repe::rest::{MEDIA_BEVE, MEDIA_JSON, MEDIA_PROBLEM, RestConfig, RestGateway};
use repe::{ErrorCode, Registry};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("response body is JSON")
    }
}

/// Write one request and read exactly one response, leaving the connection open
/// so a caller can reuse it.
async fn send(
    stream: &mut TcpStream,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut request = format!("{method} {target} HTTP/1.1\r\nHost: test\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();

    // Head first: read until the blank line that ends the header block.
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(
            read > 0,
            "connection closed before the headers were complete"
        );
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8(buffer[..head_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a parseable status line");
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();

    // Then the body, whose length the headers just told us. Every response this
    // gateway sends is a `Full` body, so there is always a Content-Length and
    // never a chunked encoding to decode.
    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    // A HEAD or 304 declares a length but sends nothing, so the declared length
    // is not what to read — read what is actually framed for this response.
    let sends_body = method != "HEAD" && status != 304;
    let mut body = buffer[head_end..].to_vec();
    if sends_body {
        while body.len() < length {
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed mid-body");
            body.extend_from_slice(&chunk[..read]);
        }
    }

    HttpResponse {
        status,
        headers,
        body,
    }
}

async fn start(config: RestConfig) -> (std::net::SocketAddr, Arc<Registry>) {
    let registry = Arc::new(Registry::new());
    registry.register_value("/counter", json!(7)).unwrap();
    registry
        .register_value("/config", json!({ "verbose": false, "name": "demo" }))
        .unwrap();
    registry
        .register_function("/add", |params| {
            let Some(Value::Object(map)) = params else {
                return Err((ErrorCode::InvalidBody, "expected an object".into()));
            };
            let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
            let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
            Ok(json!({ "result": a + b }))
        })
        .unwrap();

    let gateway = RestGateway::with_config("/api/v1", Arc::clone(&registry), config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = gateway.serve(listener).await;
    });
    (addr, registry)
}

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.unwrap()
}

#[tokio::test]
async fn a_read_carries_its_representation_and_its_cache_headers() {
    let (addr, _registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let response = send(&mut stream, "GET", "/api/v1/counter", &[], b"").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some(MEDIA_JSON));
    assert_eq!(response.header("cache-control"), Some("no-cache"));
    assert_eq!(response.header("vary"), Some("Accept"));
    assert!(response.header("etag").is_some());
    assert_eq!(response.json(), json!(7));
}

#[tokio::test]
async fn a_conditional_read_is_answered_with_304_and_no_body() {
    let (addr, _registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let first = send(&mut stream, "GET", "/api/v1/config", &[], b"").await;
    let etag = first.header("etag").unwrap().to_string();

    let second = send(
        &mut stream,
        "GET",
        "/api/v1/config",
        &[("If-None-Match", &etag)],
        b"",
    )
    .await;
    assert_eq!(second.status, 304);
    assert!(second.body.is_empty());
    assert_eq!(second.header("etag"), Some(etag.as_str()));

    // And the connection is still usable, which is the whole point of a cheap
    // revalidation: the next request rides the same connection.
    let third = send(&mut stream, "GET", "/api/v1/counter", &[], b"").await;
    assert_eq!(third.status, 200);
}

#[tokio::test]
async fn head_declares_the_length_it_does_not_send() {
    let (addr, _registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let get = send(&mut stream, "GET", "/api/v1/config", &[], b"").await;
    let head = send(&mut stream, "HEAD", "/api/v1/config", &[], b"").await;

    assert_eq!(head.status, 200);
    assert!(head.body.is_empty(), "HEAD must not send a body");
    assert_eq!(
        head.header("content-length"),
        Some(get.body.len().to_string().as_str()),
        "HEAD reports the length GET would have sent"
    );
    assert_eq!(head.header("etag"), get.header("etag"));
}

#[tokio::test]
async fn a_write_is_visible_to_the_next_read_and_to_the_registry() {
    let (addr, registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let written = send(
        &mut stream,
        "PUT",
        "/api/v1/counter",
        &[("Content-Type", MEDIA_JSON)],
        b"42",
    )
    .await;
    assert_eq!(written.status, 200);
    assert!(
        written.header("etag").is_none(),
        "a mutation carries no validator"
    );

    let read = send(&mut stream, "GET", "/api/v1/counter", &[], b"").await;
    assert_eq!(read.json(), json!(42));
    // The same state the REPE leg would serve: one registry, two front doors.
    assert_eq!(registry.read_value("/counter").unwrap(), json!(42));
}

#[tokio::test]
async fn a_call_round_trips_through_the_function() {
    let (addr, _registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let response = send(
        &mut stream,
        "POST",
        "/api/v1/add",
        &[("Content-Type", MEDIA_JSON)],
        br#"{"a":20,"b":22}"#,
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.json(), json!({ "result": 42 }));
}

#[tokio::test]
async fn beve_is_negotiated_on_both_legs() {
    // BEVE *bodies* are opt-in (see `RestConfig::accept_beve_bodies`); BEVE
    // responses are not, and this exercises both legs.
    let (addr, _registry) = start(RestConfig {
        accept_beve_bodies: true,
        ..RestConfig::default()
    })
    .await;
    let mut stream = connect(addr).await;

    let read = send(
        &mut stream,
        "GET",
        "/api/v1/config",
        &[("Accept", MEDIA_BEVE)],
        b"",
    )
    .await;
    assert_eq!(read.header("content-type"), Some(MEDIA_BEVE));
    let decoded: Value = beve::from_slice(&read.body).unwrap();
    assert_eq!(decoded["name"], json!("demo"));

    let body = beve::to_vec(&json!({ "a": 1, "b": 2 })).unwrap();
    let call = send(
        &mut stream,
        "POST",
        "/api/v1/add",
        &[("Content-Type", MEDIA_BEVE), ("Accept", MEDIA_BEVE)],
        &body,
    )
    .await;
    assert_eq!(call.status, 200);
    let decoded: Value = beve::from_slice(&call.body).unwrap();
    assert_eq!(decoded["result"], json!(3));
}

#[tokio::test]
async fn a_verb_the_target_does_not_support_is_405_with_allow() {
    let (addr, registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let response = send(
        &mut stream,
        "POST",
        "/api/v1/counter",
        &[("Content-Type", MEDIA_JSON)],
        b"1",
    )
    .await;
    assert_eq!(response.status, 405);
    assert_eq!(response.header("allow"), Some("GET, HEAD, PUT, OPTIONS"));
    assert_eq!(
        registry.read_value("/counter").unwrap(),
        json!(7),
        "the refused request must not have taken effect"
    );

    let options = send(&mut stream, "OPTIONS", "/api/v1/add", &[], b"").await;
    assert_eq!(options.status, 204);
    assert_eq!(options.header("allow"), Some("GET, HEAD, POST, OPTIONS"));
}

#[tokio::test]
async fn a_missing_resource_is_problem_details_carrying_the_repe_code() {
    let (addr, _registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let response = send(&mut stream, "GET", "/api/v1/absent", &[], b"").await;
    assert_eq!(response.status, 404);
    assert_eq!(response.header("content-type"), Some(MEDIA_PROBLEM));
    assert_eq!(
        response.header("x-repe-error-code"),
        Some((ErrorCode::MethodNotFound as u32).to_string().as_str())
    );
    assert_eq!(response.json()["status"], json!(404));
}

#[tokio::test]
async fn an_oversized_body_is_refused_before_it_is_buffered() {
    let (addr, registry) = start(RestConfig {
        max_body_bytes: 32,
        ..RestConfig::default()
    })
    .await;
    let mut stream = connect(addr).await;

    let oversized = format!("\"{}\"", "x".repeat(256));
    let response = send(
        &mut stream,
        "PUT",
        "/api/v1/counter",
        &[("Content-Type", MEDIA_JSON)],
        oversized.as_bytes(),
    )
    .await;
    assert_eq!(response.status, 413);
    assert_eq!(registry.read_value("/counter").unwrap(), json!(7));

    // A body just under the limit still goes through, so the limit is a limit
    // and not an outage.
    let mut stream = connect(addr).await;
    let accepted = send(
        &mut stream,
        "PUT",
        "/api/v1/counter",
        &[("Content-Type", MEDIA_JSON)],
        b"\"small\"",
    )
    .await;
    assert_eq!(accepted.status, 200);
}

#[tokio::test]
async fn one_connection_serves_many_requests() {
    // Keep-alive is what makes the facade cheap enough to sit in front of a
    // binary protocol at all; a per-request connection would dominate everything
    // the mapping does.
    let (addr, _registry) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    for expected in 0..5i64 {
        send(
            &mut stream,
            "PUT",
            "/api/v1/counter",
            &[("Content-Type", MEDIA_JSON)],
            expected.to_string().as_bytes(),
        )
        .await;
        let read = send(&mut stream, "GET", "/api/v1/counter", &[], b"").await;
        assert_eq!(read.json(), json!(expected));
    }
}
