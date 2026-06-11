//! End-to-end tests for the async SVS pull driver: a synchronous `Server`
//! producing a streamed value (it honours `Execution::OffReader`) and an
//! `AsyncClient` pulling it from async code, across the async/blocking decode
//! boundary, over a real TCP loopback connection. Covers the value, typed-slice,
//! and complex-slice receivers, each compressed and uncompressed.
#![cfg(feature = "value-stream")]

use repe::value_stream::{Compression, RouterValueStreamExt, StreamOpts};
use repe::{
    AsyncClient, Complex, Router, Server, pull_complex_slice_async, pull_typed_slice_async,
    pull_value_async,
};
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, TcpListener};
use std::thread;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Payload {
    id: u64,
    label: String,
    samples: Vec<f64>,
    tags: Vec<String>,
}

fn sample_payload() -> Payload {
    // ~1.6 MiB, spans many 16 KiB chunks so the pull loop / lookahead / blocking
    // decode bridge are exercised across multiple `next` round trips.
    Payload {
        id: 0xFEED_BEEF,
        label: "serialized value stream".to_string(),
        samples: (0..200_000).map(|i| (i as f64) * 0.5 - 1234.5).collect(),
        tags: (0..64).map(|i| format!("tag-{i:03}")).collect(),
    }
}

fn f64_samples() -> Vec<f64> {
    (0..200_000).map(|i| (i as f64) * 0.5 - 1234.5).collect()
}

fn iq_samples() -> Vec<Complex<f32>> {
    (0..200_000)
        .map(|i| Complex {
            re: (i as f32) * 1e-3,
            im: -(i as f32) * 2e-3,
        })
        .collect()
}

fn small_chunks(compression: Compression) -> StreamOpts {
    StreamOpts {
        chunk_bytes: 16 * 1024,
        compression,
        zstd_level: 3,
        session_depth: 4,
    }
}

/// Spawn a detached sync `Server` for `router` on an ephemeral port and return
/// its address (the sync server honours `OffReader`, which the SVS `next`
/// handler reports).
fn spawn(router: Router) -> SocketAddr {
    let server = Server::new(router);
    let listener: TcpListener = server.listen("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let _ = server.serve(listener);
    });
    addr
}

async fn run_value(compression: Compression) {
    let payload = sample_payload();
    let p = payload.clone();
    let addr = spawn(Router::new().with_value_stream(
        move |resource: &str| (resource == "payload").then(|| p.clone()),
        small_chunks(compression),
    ));
    let client = AsyncClient::connect(addr).await.expect("connect");
    let got: Payload = pull_value_async(&client, "payload").await.expect("pull");
    assert_eq!(got, payload);
}

#[tokio::test]
async fn value_mode_roundtrips_uncompressed() {
    run_value(Compression::None).await;
}

#[tokio::test]
async fn value_mode_roundtrips_compressed() {
    run_value(Compression::Zstd).await;
}

async fn run_typed(compression: Compression) {
    let v = f64_samples();
    let vc = v.clone();
    let addr = spawn(Router::new().with_typed_value_stream(
        move |resource: &str| (resource == "buf").then(|| vc.clone()),
        small_chunks(compression),
    ));
    let client = AsyncClient::connect(addr).await.expect("connect");
    let got: Vec<f64> = pull_typed_slice_async(&client, "buf").await.expect("pull");
    assert_eq!(got, v);
}

#[tokio::test]
async fn typed_slice_roundtrips_uncompressed() {
    run_typed(Compression::None).await;
}

#[tokio::test]
async fn typed_slice_roundtrips_compressed() {
    run_typed(Compression::Zstd).await;
}

async fn run_complex(compression: Compression) {
    let v = iq_samples();
    let vc = v.clone();
    let addr = spawn(Router::new().with_complex_value_stream(
        move |resource: &str| (resource == "iq").then(|| vc.clone()),
        small_chunks(compression),
    ));
    let client = AsyncClient::connect(addr).await.expect("connect");
    let got: Vec<Complex<f32>> = pull_complex_slice_async(&client, "iq").await.expect("pull");
    assert_eq!(got, v);
}

#[tokio::test]
async fn complex_slice_roundtrips_uncompressed() {
    run_complex(Compression::None).await;
}

#[tokio::test]
async fn complex_slice_roundtrips_compressed() {
    run_complex(Compression::Zstd).await;
}

/// A missing resource must surface as an error, not hang.
#[tokio::test]
async fn unknown_resource_errors() {
    let addr = spawn(Router::new().with_value_stream(
        move |resource: &str| (resource == "payload").then(sample_payload),
        small_chunks(Compression::None),
    ));
    let client = AsyncClient::connect(addr).await.expect("connect");
    let got = pull_value_async::<Payload, _>(&client, "does-not-exist").await;
    assert!(got.is_err(), "unknown resource must error");
}

// The same async pullers drive a `WebSocketClient`. The WebSocket server runs
// the SVS `next` handler off the reader task (it honours `Execution::OffReader`),
// so blocking for backpressure there is fine.
#[cfg(feature = "websocket")]
mod websocket {
    use super::*;
    use repe::{WebSocketClient, WebSocketServer};

    /// Spawn a detached WebSocket server for `router` on path `/repe` and return a
    /// connected client.
    async fn ws_client(router: Router) -> WebSocketClient {
        let listener = WebSocketServer::listen("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = WebSocketServer::new(router)
                .serve_listener(listener, "/repe")
                .await;
        });
        WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .expect("connect")
    }

    #[tokio::test]
    async fn value_mode_over_websocket() {
        let payload = sample_payload();
        let p = payload.clone();
        let client = ws_client(Router::new().with_value_stream(
            move |resource: &str| (resource == "payload").then(|| p.clone()),
            small_chunks(Compression::Zstd),
        ))
        .await;
        let got: Payload = pull_value_async(&client, "payload").await.expect("pull");
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn typed_slice_over_websocket() {
        let v = f64_samples();
        let vc = v.clone();
        let client = ws_client(Router::new().with_typed_value_stream(
            move |resource: &str| (resource == "buf").then(|| vc.clone()),
            small_chunks(Compression::None),
        ))
        .await;
        let got: Vec<f64> = pull_typed_slice_async(&client, "buf").await.expect("pull");
        assert_eq!(got, v);
    }

    #[tokio::test]
    async fn complex_slice_over_websocket() {
        let v = iq_samples();
        let vc = v.clone();
        let client = ws_client(Router::new().with_complex_value_stream(
            move |resource: &str| (resource == "iq").then(|| vc.clone()),
            small_chunks(Compression::Zstd),
        ))
        .await;
        let got: Vec<Complex<f32>> = pull_complex_slice_async(&client, "iq").await.expect("pull");
        assert_eq!(got, v);
    }
}
