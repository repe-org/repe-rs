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
use repe::structs::RequestBody;
use repe::{ErrorCode, Registry};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Default, Debug, PartialEq)]
struct Operands {
    a: i64,
    b: i64,
}
structio::object!(Operands { a, b });

#[derive(Default, Debug, PartialEq)]
struct Sum {
    result: i64,
}
structio::object!(Sum { result });

#[derive(Default, Debug, PartialEq)]
struct Config {
    verbose: bool,
    name: String,
}
structio::object!(Config { verbose, name });

/// The one problem-details member this file asserts on. Read under
/// [`SkipUnknown`](structio::SkipUnknown), because the gateway sends the full
/// RFC 9457 object and a declaration that names one key would otherwise refuse
/// the rest of it: `..` says the *type* has more fields, not that the document
/// may.
#[derive(Default, Debug, PartialEq)]
struct Problem {
    status: u16,
}
structio::object!(Problem { status, .. });

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

    /// The body decoded as `T`.
    fn json<T: repe::structs::ServableOwned>(&self) -> T {
        structio::from_slice(&self.body).expect("response body is JSON")
    }

    /// The body as JSON text, for the cases where the shape is the assertion.
    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("response body is UTF-8 JSON")
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

    read_response(stream, method).await
}

/// Read exactly one response off `stream`. Split out from [`send`] so a test
/// can write its own request bytes — or deliberately fail to.
async fn read_response(stream: &mut TcpStream, method: &str) -> HttpResponse {
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

/// Every registered path is a function now, so `/counter` is one too: the
/// handler owns the state and decides what a body means. That is how a value
/// endpoint is spelled with no document store behind the registry.
async fn start(config: RestConfig) -> (std::net::SocketAddr, Arc<AtomicI64>) {
    let registry = Arc::new(Registry::new());

    let counter = Arc::new(AtomicI64::new(7));
    let handle = Arc::clone(&counter);
    registry
        .register_function("/counter", move |params: Option<RequestBody<'_>>| {
            if let Some(body) = params {
                let next: i64 = body
                    .read("/counter")
                    .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
                counter.store(next, Ordering::SeqCst);
            }
            Ok(counter.load(Ordering::SeqCst))
        })
        .unwrap();

    registry
        .register_function("/config", |_: Option<RequestBody<'_>>| {
            Ok(Config {
                verbose: false,
                name: "demo".into(),
            })
        })
        .unwrap();

    registry
        .register_function("/add", |params: Option<RequestBody<'_>>| {
            let Some(body) = params else {
                return Err((ErrorCode::InvalidBody, "expected an object".into()));
            };
            let operands: Operands = body
                .read("/add")
                .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
            Ok(Sum {
                result: operands.a + operands.b,
            })
        })
        .unwrap();

    let gateway = RestGateway::with_config("/api/v1", registry, config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = gateway.serve(listener).await;
    });
    (addr, handle)
}

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.unwrap()
}

#[tokio::test]
async fn a_read_carries_its_representation_and_its_cache_headers() {
    let (addr, _counter) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let response = send(&mut stream, "GET", "/api/v1/counter", &[], b"").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some(MEDIA_JSON));
    assert_eq!(response.header("cache-control"), Some("no-cache"));
    assert_eq!(response.header("vary"), Some("Accept"));
    assert!(response.header("etag").is_some());
    assert_eq!(response.json::<i64>(), 7);
}

#[tokio::test]
async fn a_conditional_read_is_answered_with_304_and_no_body() {
    let (addr, _counter) = start(RestConfig::default()).await;
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
    let (addr, _counter) = start(RestConfig::default()).await;
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
    let (addr, counter) = start(RestConfig::default()).await;
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
    assert_eq!(read.json::<i64>(), 42);
    // The same state the REPE leg would serve: one handler, two front doors.
    assert_eq!(counter.load(Ordering::SeqCst), 42);
}

#[tokio::test]
async fn a_call_round_trips_through_the_function() {
    let (addr, _counter) = start(RestConfig::default()).await;
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
    assert_eq!(response.json::<Sum>(), Sum { result: 42 });
}

#[tokio::test]
async fn beve_is_negotiated_on_both_legs() {
    let (addr, _counter) = start(RestConfig::default()).await;
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
    let decoded: Config = structio::from_beve(&read.body).unwrap();
    assert_eq!(decoded.name, "demo");

    let body = structio::to_beve(&Operands { a: 1, b: 2 });
    let call = send(
        &mut stream,
        "POST",
        "/api/v1/add",
        &[("Content-Type", MEDIA_BEVE), ("Accept", MEDIA_BEVE)],
        &body,
    )
    .await;
    assert_eq!(call.status, 200);
    let decoded: Sum = structio::from_beve(&call.body).unwrap();
    assert_eq!(decoded, Sum { result: 3 });
}

#[tokio::test]
async fn put_and_post_both_call_and_a_third_verb_is_405() {
    // The registry stores no values, so there is no assignment for `PUT` to
    // mean instead of a call. The two verbs are documented aliases and every
    // registered path admits both; anything else is 405 with the same `Allow`.
    let (addr, counter) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    for (verb, value) in [("PUT", 1i64), ("POST", 2)] {
        let response = send(
            &mut stream,
            verb,
            "/api/v1/counter",
            &[("Content-Type", MEDIA_JSON)],
            value.to_string().as_bytes(),
        )
        .await;
        assert_eq!(response.status, 200, "{verb}");
        assert_eq!(counter.load(Ordering::SeqCst), value, "{verb}");
    }

    let response = send(&mut stream, "DELETE", "/api/v1/counter", &[], b"").await;
    assert_eq!(response.status, 405);
    assert_eq!(
        response.header("allow"),
        Some("GET, HEAD, PUT, POST, OPTIONS")
    );

    let options = send(&mut stream, "OPTIONS", "/api/v1/add", &[], b"").await;
    assert_eq!(options.status, 204);
    assert_eq!(
        options.header("allow"),
        Some("GET, HEAD, PUT, POST, OPTIONS")
    );
}

#[tokio::test]
async fn a_missing_resource_is_problem_details_carrying_the_repe_code() {
    let (addr, _counter) = start(RestConfig::default()).await;
    let mut stream = connect(addr).await;

    let response = send(&mut stream, "GET", "/api/v1/absent", &[], b"").await;
    assert_eq!(response.status, 404);
    assert_eq!(response.header("content-type"), Some(MEDIA_PROBLEM));
    assert_eq!(
        response.header("x-repe-error-code"),
        Some((ErrorCode::MethodNotFound as u32).to_string().as_str())
    );
    let problem: Problem =
        structio::json::from_slice_with::<structio::SkipUnknown, _>(&response.body)
            .expect("the problem body is JSON");
    assert_eq!(problem.status, 404);
}

#[tokio::test]
async fn an_oversized_body_is_refused_before_it_is_buffered() {
    let (addr, counter) = start(RestConfig {
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
    assert_eq!(counter.load(Ordering::SeqCst), 7);

    // A body just under the limit still goes through, so the limit is a limit
    // and not an outage.
    let mut stream = connect(addr).await;
    let accepted = send(
        &mut stream,
        "PUT",
        "/api/v1/counter",
        &[("Content-Type", MEDIA_JSON)],
        b"11",
    )
    .await;
    assert_eq!(accepted.status, 200);
    assert_eq!(counter.load(Ordering::SeqCst), 11);
}

#[tokio::test]
async fn one_connection_serves_many_requests() {
    // Keep-alive is what makes the facade cheap enough to sit in front of a
    // binary protocol at all; a per-request connection would dominate everything
    // the mapping does.
    let (addr, _counter) = start(RestConfig::default()).await;
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
        assert_eq!(read.json::<i64>(), expected);
    }
}

/// A client that promises a body and never sends it must not hold its
/// connection — and its slot under `max_connections` — indefinitely.
///
/// Without a bound on the body read, a few hundred sockets costing an attacker
/// almost nothing stop the gateway from accepting anything at all. The header
/// timeout does not cover this: the head here is complete and well-formed.
#[tokio::test]
async fn a_promised_body_that_never_arrives_is_timed_out() {
    let (addr, _counter) = start(RestConfig {
        request_timeout: Some(std::time::Duration::from_millis(300)),
        ..RestConfig::default()
    })
    .await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            b"PUT /api/v1/counter HTTP/1.1\r\nHost: test\r\n\
              Content-Type: application/json\r\nContent-Length: 100\r\n\r\n",
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();

    // No body follows. The gateway must answer on its own rather than wait.
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_response(&mut stream, "PUT"),
    )
    .await
    .expect("the gateway held the connection past its own request timeout");
    assert_eq!(response.status, 408);
    assert_eq!(
        response.header("connection"),
        Some("close"),
        "the promised body is still unread, so the stream is not at a message boundary"
    );
}

/// The slot that request took must come back, or a handful of stalled clients
/// would still be an outage even with the timeout in place.
#[tokio::test]
async fn a_timed_out_connection_releases_its_slot() {
    let (addr, _counter) = start(RestConfig {
        request_timeout: Some(std::time::Duration::from_millis(200)),
        max_connections: 2,
        ..RestConfig::default()
    })
    .await;

    let mut stalled = Vec::new();
    for _ in 0..2 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"PUT /api/v1/counter HTTP/1.1\r\nHost: test\r\n\
                  Content-Type: application/json\r\nContent-Length: 100\r\n\r\n",
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();
        stalled.push(stream);
    }

    // Every slot is now held by a client that will never finish its request.
    // Once they time out, an ordinary read has to get through.
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        send(&mut stream, "GET", "/api/v1/counter", &[], b"").await
    })
    .await
    .expect("stalled connections kept the gateway from serving");
    assert_eq!(response.status, 200);
}
