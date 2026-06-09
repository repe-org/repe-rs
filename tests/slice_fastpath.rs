//! End-to-end coverage for the bulk numeric fast path: `Router::with_slice`
//! paired with `AsyncClient::call_slice`, plus wire-interop with the serde
//! (`with_typed` / `call_typed_beve`) path.

#![cfg(not(target_arch = "wasm32"))]

use repe::server::Router;
use repe::{AsyncClient, AsyncServer, Client, Server};
use std::time::Duration;

async fn spawn(router: Router) -> String {
    let listener = AsyncServer::listen("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    tokio::spawn(async move { AsyncServer::new(router).serve(listener).await });
    addr
}

/// Start a synchronous `Server` on an ephemeral port in a background thread and
/// return its address, for exercising the blocking `Client::call_slice` path.
fn spawn_sync(router: Router) -> String {
    let server = Server::new(router);
    let listener = server.listen("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || server.serve(listener));
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_slice_roundtrips_f64_and_f32() {
    let router = Router::new()
        .with_slice::<f64, f64, _>("/scale2", |xs| Ok(xs.iter().map(|x| x * 2.0).collect()))
        .with_slice::<f32, f32, _>("/negate", |xs| Ok(xs.iter().map(|x| -x).collect()));
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let xs: Vec<f64> = (0..4096).map(|i| i as f64 * 0.25).collect();
    let out: Vec<f64> = client.call_slice("/scale2", &xs).await.expect("call f64");
    assert_eq!(out, xs.iter().map(|x| x * 2.0).collect::<Vec<_>>());

    let fs: Vec<f32> = (0..1024).map(|i| i as f32 - 7.5).collect();
    let neg: Vec<f32> = client.call_slice("/negate", &fs).await.expect("call f32");
    assert_eq!(neg, fs.iter().map(|x| -x).collect::<Vec<_>>());

    // Empty slice: a zero-length typed array must round-trip cleanly (the
    // length-prefix-only body the bulk writer emits, read back as an empty Vec).
    let empty: Vec<f64> = Vec::new();
    let back: Vec<f64> = client.call_slice("/scale2", &empty).await.expect("empty");
    assert!(back.is_empty());

    // Timeout-bearing variant succeeds on the happy path.
    let xs2: Vec<f64> = vec![1.0, 2.0, 3.0];
    let out2: Vec<f64> = client
        .call_slice_with_timeout("/scale2", &xs2, Duration::from_secs(5))
        .await
        .expect("call_slice_with_timeout");
    assert_eq!(out2, vec![2.0, 4.0, 6.0]);
}

#[test]
fn sync_client_call_slice_roundtrips() {
    // The blocking `Client::call_slice` path against a synchronous `Server`.
    let router = Router::new()
        .with_slice::<f64, f64, _>("/scale2", |xs| Ok(xs.iter().map(|x| x * 2.0).collect()));
    let addr = spawn_sync(router);

    let client = Client::connect(&addr).expect("connect");
    let xs: Vec<f64> = (0..2048).map(|i| i as f64 * 0.5).collect();
    let out: Vec<f64> = client.call_slice("/scale2", &xs).expect("sync call_slice");
    assert_eq!(out, xs.iter().map(|x| x * 2.0).collect::<Vec<_>>());

    let out2: Vec<f64> = client
        .call_slice_with_timeout("/scale2", &xs, Duration::from_secs(5))
        .expect("sync call_slice_with_timeout");
    assert_eq!(out2, xs.iter().map(|x| x * 2.0).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slice_route_interops_with_serde_client() {
    // A with_slice route decodes a body sent through the serde path
    // (`call_typed_beve` over a `Vec<f64>`), because the wire bytes are identical.
    let router = Router::new().with_slice::<f64, f64, _>("/echo", Ok);
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let xs: Vec<f64> = vec![1.0, -2.5, 3.25, 4.0e9];
    // serde request -> bulk decode on the server -> bulk response -> serde decode.
    let out: Vec<f64> = client
        .call_typed_beve("/echo", &xs)
        .await
        .expect("serde->slice");
    assert_eq!(out, xs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serde_route_interops_with_slice_client() {
    // The mirror: a serde `with_typed` route decodes a body sent through the bulk
    // `call_slice` path, and the bulk client decodes the serde-framed response.
    let router = Router::new().with_typed::<Vec<f64>, Vec<f64>, _>("/echo", |xs: Vec<f64>| {
        Ok(repe::server::TypedResponse::beve(xs))
    });
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let xs: Vec<f64> = (0..256).map(|i| i as f64 * 1.5).collect();
    let out: Vec<f64> = client.call_slice("/echo", &xs).await.expect("slice->serde");
    assert_eq!(out, xs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_element_type_is_an_error() {
    // Server expects f64; client sends f32. The element class/width disagree, so
    // the bulk decoder rejects rather than misreading.
    let router = Router::new().with_slice::<f64, f64, _>("/echo", Ok);
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let fs: Vec<f32> = vec![1.0, 2.0, 3.0];
    let res: Result<Vec<f32>, _> = client.call_slice("/echo", &fs).await;
    assert!(res.is_err(), "f32 into an f64 route must error");
}
