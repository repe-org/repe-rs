//! Browser tests for [`repe::WasmClient`], run under `wasm-bindgen-test`.
//!
//! `src/wasm_client.rs` is gated on `target_arch = "wasm32"`, so the host suite
//! never compiles it and CI can only prove it builds. These tests drive it in a
//! real browser against a real `WebSocket`, which is the only way to cover the
//! JS callback wiring -- where both of this module's known bugs lived.
//!
//! Requires the scripted server in `wasm-tests/server`; see
//! `wasm-tests/README.md` for how to run the pair.

#![cfg(target_arch = "wasm32")]

use futures_util::future::{Either, select};
use futures_util::{Stream, StreamExt};
use gloo_timers::future::TimeoutFuture;
use repe::{AlreadySubscribed, Message, WasmClient};
use std::future::Future;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_browser);

/// Kept in step with `DEFAULT_PORT` in `wasm-tests/server/src/main.rs`.
const SERVER_URL: &str = match option_env!("REPE_WASM_TEST_URL") {
    Some(url) => url,
    None => "ws://127.0.0.1:8791",
};

async fn connect() -> WasmClient {
    WasmClient::connect(SERVER_URL)
        .await
        .expect("the scripted test server should be running; see wasm-tests/README.md")
}

/// Generous enough that no correct run can reach it. The whole suite finishes
/// in under a tenth of this against a loopback socket, so it races nothing it
/// could lose; it exists only to convert a hang into a legible failure.
const TIMEOUT_MS: u32 = 10_000;

/// Bound a wait so a regression fails loudly instead of hanging.
///
/// Worth the machinery because `wasm-bindgen-test` handles a hung test badly:
/// it reports "Failed to detect test as having been run", names no assertion,
/// and abandons the rest of the suite, so one stuck test hides every result
/// behind it. Verified: without it, dropping the end-of-stream fix takes the
/// whole run down with no indication of which test was stuck.
async fn within_timeout<T>(fut: impl Future<Output = T> + Unpin, waiting_for: &str) -> T {
    match select(fut, TimeoutFuture::new(TIMEOUT_MS)).await {
        Either::Left((value, _)) => value,
        Either::Right(_) => panic!("timed out after {TIMEOUT_MS}ms waiting for {waiting_for}"),
    }
}

/// Await one message from the notify stream, failing rather than hanging if the
/// stream ends or stalls.
async fn expect_notify(notifies: &mut (impl Stream<Item = Message> + Unpin)) -> Message {
    within_timeout(notifies.next(), "a notify")
        .await
        .expect("the notify stream ended instead of yielding a message")
}

#[wasm_bindgen_test]
async fn a_json_call_round_trips() {
    // Baseline. If this fails, every other failure here is downstream of it.
    let client = connect().await;

    let echoed = client
        .call_json("/echo", &serde_json::json!({ "hello": "browser" }))
        .await
        .expect("call should succeed");

    assert_eq!(echoed, serde_json::json!({ "hello": "browser" }));
}

#[wasm_bindgen_test]
async fn a_pushed_notify_reaches_the_subscriber() {
    // The headline feature: before this existed, the client matched every frame
    // by request id and dropped this one.
    let client = connect().await;
    let mut notifies = client.subscribe_notifies().expect("subscribe");

    client
        .call_json("/notify-then-respond", &serde_json::json!({}))
        .await
        .expect("call should succeed");

    let pushed = expect_notify(&mut notifies).await;
    assert_eq!(pushed.query_utf8(), "/pushed");
    assert!(pushed.header.notify != 0);
    assert_eq!(
        pushed.json_body::<serde_json::Value>().unwrap(),
        serde_json::json!({ "seq": 1 })
    );
}

#[wasm_bindgen_test]
async fn a_notify_sharing_a_request_id_does_not_steal_the_response() {
    // The reason the notify flag is checked before the correlation map. The
    // server sends a notify carrying the in-flight request's own id; a client
    // that looked up `pending` first would hand the notify to the caller as its
    // response, then find no waiter left for the real one.
    let client = connect().await;
    let mut notifies = client.subscribe_notifies().expect("subscribe");

    let response = client
        .call_json("/collide", &serde_json::json!({}))
        .await
        .expect("the real response should still arrive");

    assert_eq!(response, serde_json::json!({ "ok": true }));

    let collided = expect_notify(&mut notifies).await;
    assert_eq!(collided.query_utf8(), "/collided");
}

#[wasm_bindgen_test]
async fn an_undecodable_frame_does_not_end_the_subscription() {
    // A decode failure is not connection death: the socket stays open and the
    // next frame may be a perfectly good notify. Ending the stream here would
    // tell the application to reconnect over a healthy connection.
    let client = connect().await;
    let mut notifies = client.subscribe_notifies().expect("subscribe");

    client
        .call_json("/undecodable-then-notify", &serde_json::json!({}))
        .await
        .expect("call should succeed");

    let after = expect_notify(&mut notifies).await;
    assert_eq!(after.query_utf8(), "/after-undecodable");
}

#[wasm_bindgen_test]
async fn the_stream_ends_when_the_server_closes() {
    // A push-only page never issues a request, so it never sees a transport
    // error. End-of-stream is its only signal to reconnect. The client is held
    // alive here so the stream must end because the socket died, not because
    // the last handle dropped.
    let client = connect().await;
    let mut notifies = client.subscribe_notifies().expect("subscribe");

    client
        .call_json("/close-after-response", &serde_json::json!({}))
        .await
        .expect("call should succeed");

    let ended = within_timeout(notifies.next(), "the stream to end").await;
    assert!(
        ended.is_none(),
        "the stream should end when the socket closes, got {ended:?}"
    );
}

#[wasm_bindgen_test]
async fn a_second_subscribe_is_refused_and_unsubscribe_hands_the_slot_back() {
    // `WasmClient` is `Clone`, so a silent replace would let one holder steal
    // another's stream. Refusing loudly is what makes the handoff explicit.
    let client = connect().await;
    let first = client.subscribe_notifies().expect("first subscribe");

    assert_eq!(
        client.subscribe_notifies().unwrap_err(),
        AlreadySubscribed,
        "a live subscription should not be displaced"
    );
    assert_eq!(
        client.clone().subscribe_notifies().unwrap_err(),
        AlreadySubscribed,
        "cloning the client should not sneak past the contract"
    );

    client.unsubscribe_notifies();
    let _second = client
        .subscribe_notifies()
        .expect("the slot should be free after unsubscribe");

    // Held to the end so this exercises the explicit-handoff path rather than
    // the stale-slot path, which only applies once the receiver is dropped.
    drop(first);
}

#[wasm_bindgen_test]
async fn notifies_sent_before_subscribing_are_dropped() {
    // Subscribers attach lazily and nothing is buffered for them, so a notify
    // that arrives first is gone. The round-trip guarantees ordering: when the
    // call returns, the server's earlier notify has already been processed.
    let client = connect().await;

    client
        .call_json("/notify-then-respond", &serde_json::json!({}))
        .await
        .expect("call should succeed");

    let mut notifies = client.subscribe_notifies().expect("subscribe");

    // The second round-trip's notify is the first this subscriber can see; if
    // the earlier one had been queued it would arrive here instead.
    client
        .call_json("/collide", &serde_json::json!({}))
        .await
        .expect("call should succeed");

    let observed = expect_notify(&mut notifies).await;
    assert_eq!(
        observed.query_utf8(),
        "/collided",
        "the pre-subscription notify should not have been replayed"
    );
}
