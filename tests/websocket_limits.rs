#![cfg(feature = "websocket")]
//! End-to-end behavior of [`repe::WebSocketLimits`] over a real socket.
//!
//! The failure these guard against is not a wrong answer, it is a *missing*
//! one: an oversized message leaves the sender cleanly and the peer's reader
//! closes the connection, so the sender sees a dropped socket rather than an
//! error. Every test here therefore asserts on what the caller observes, not on
//! any internal state.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use repe::server::Router;
use repe::{RepeError, WebSocketClient, WebSocketLimits, WebSocketServer};
use serde_json::{Value, json};

/// Spawn a detached server for `router` on an ephemeral port and connect a
/// client, both with the given limits.
async fn pair(
    router: Router,
    server_limits: WebSocketLimits,
    client_limits: WebSocketLimits,
) -> WebSocketClient {
    let listener = WebSocketServer::listen("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = WebSocketServer::new(router)
            .with_limits(server_limits)
            .serve_listener(listener, "/repe")
            .await;
    });
    WebSocketClient::connect_with_limits(&format!("ws://{addr}/repe"), client_limits)
        .await
        .expect("connect")
}

/// A router whose one route returns a body of `len` bytes.
fn echo_sized_router() -> Router {
    Router::new().with_json("/blob", |payload: Value| {
        let len = payload.get("len").and_then(Value::as_u64).unwrap_or(0) as usize;
        Ok(json!({ "data": "x".repeat(len) }))
    })
}

/// The headline case: a response the client could never have read arrives as a
/// REPE error instead of a closed connection, and the connection survives to
/// serve the next request.
#[tokio::test]
async fn an_oversized_response_becomes_an_error_and_the_connection_survives() {
    let small = WebSocketLimits::default()
        .with_max_incoming_frame_size(Some(64 * 1024))
        .with_max_incoming_message_size(Some(64 * 1024))
        .with_assumed_peer_frame_limit(Some(64 * 1024));
    let client = pair(echo_sized_router(), small, small).await;

    let err = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "len": 256 * 1024 }))
        .await
        .expect_err("an undeliverable response must not look like success");
    let text = err.to_string();
    assert!(
        text.contains("assumed peer frame limit"),
        "the error should explain what happened, got: {text}"
    );

    // The point of substituting an error rather than sending: the socket is
    // still usable. A torn-down connection would fail this.
    let ok: Value = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "len": 8 }))
        .await
        .expect("connection must survive a refused response");
    assert_eq!(ok["data"], "xxxxxxxx");
}

/// The refusal is reported to the server's own error hooks too, so an operator
/// can see it without a client complaining.
#[tokio::test]
async fn the_server_reports_a_refused_response_through_its_error_hooks() {
    let small = WebSocketLimits::default().with_assumed_peer_frame_limit(Some(4096));
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_hook = Arc::clone(&seen);

    let listener = WebSocketServer::listen("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = WebSocketServer::new(echo_sized_router())
            .with_limits(small)
            .on_error(move |err| {
                if matches!(err, repe::ConnectionError::OutboundTooLarge { .. }) {
                    seen_hook.fetch_add(1, Ordering::SeqCst);
                }
            })
            .serve_listener(listener, "/repe")
            .await;
    });
    let client = WebSocketClient::connect_with_limits(&format!("ws://{addr}/repe"), small)
        .await
        .expect("connect");

    let _ = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "len": 64 * 1024 }))
        .await;
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the refusal should reach on_error exactly once"
    );
}

/// A client request too large for the server is refused locally, before it hits
/// the wire, so the client keeps its connection instead of losing it silently.
#[tokio::test]
async fn an_oversized_request_is_refused_locally_without_dropping_the_connection() {
    let small = WebSocketLimits::default().with_assumed_peer_frame_limit(Some(8192));
    let client = pair(echo_sized_router(), small, small).await;

    let err = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "pad": "y".repeat(32 * 1024) }))
        .await
        .expect_err("an oversized request must be refused");
    assert!(
        matches!(err, RepeError::MessageTooLarge { .. }),
        "expected MessageTooLarge, got: {err}"
    );

    let ok: Value = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "len": 4 }))
        .await
        .expect("connection must survive a locally refused request");
    assert_eq!(ok["data"], "xxxx");
}

/// Raising the limits on both ends carries a payload that the defaults refuse,
/// which is the whole reason the knob exists.
#[tokio::test]
async fn raising_the_limits_on_both_ends_carries_a_payload_the_defaults_refuse() {
    // Comfortably over the 16 MiB default so the default would refuse it.
    let big_len = 20 * 1024 * 1024;
    let generous = WebSocketLimits::default()
        .with_max_incoming_frame_size(Some(64 << 20))
        .with_max_incoming_message_size(Some(64 << 20))
        .with_assumed_peer_frame_limit(Some(64 << 20));

    // The default refuses it...
    let strict = pair(echo_sized_router(), Default::default(), Default::default()).await;
    assert!(
        strict
            .call_typed_json::<_, _, Value>("/blob", &json!({ "len": big_len }))
            .await
            .is_err(),
        "the default limits should refuse a 20 MiB response"
    );

    // ...and raising both ends carries it.
    let client = pair(echo_sized_router(), generous, generous).await;
    let got: Value = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "len": big_len }))
        .await
        .expect("raised limits should carry a 20 MiB response");
    assert_eq!(got["data"].as_str().expect("data").len(), big_len);
}

/// Turning the guard off restores the unguarded behavior, for a peer whose
/// limits are known to be absent.
#[tokio::test]
async fn clearing_the_assumed_peer_limit_sends_without_checking() {
    let unguarded = WebSocketLimits::default().with_assumed_peer_frame_limit(None);
    // The client sends without checking; the server accepts it because its own
    // inbound thresholds are the generous defaults.
    let client = pair(echo_sized_router(), Default::default(), unguarded).await;
    let got: Value = client
        .call_typed_json::<_, _, Value>("/blob", &json!({ "len": 16 }))
        .await
        .expect("a request under the peer's inbound limit still works");
    assert_eq!(got["data"].as_str().expect("data").len(), 16);
}
