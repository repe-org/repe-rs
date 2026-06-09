//! End-to-end coverage for the zero-copy borrowing route: `Router::with_typed_slice_ref`
//! paired with `*::call_typed_slice_aligned` (the aligned wire form), plus the
//! interop guarantees that make a `_ref` route a drop-in superset of
//! `with_typed_slice`.

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

fn spawn_sync(router: Router) -> String {
    let server = Server::new(router);
    let listener = server.listen("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    std::thread::spawn(move || server.serve(listener));
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aligned_call_roundtrips_against_ref_route() {
    let router = Router::new()
        .with_typed_slice_ref::<f64, f64, _>("/scale2", |xs| {
            Ok(xs.iter().map(|x| x * 2.0).collect())
        })
        .with_typed_slice_ref::<f32, f32, _>("/negate", |xs| Ok(xs.iter().map(|x| -x).collect()));
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    // A large body spans multiple TCP segments, exercising the borrow path on the
    // server's reused, (in practice) aligned receive buffer.
    let xs: Vec<f64> = (0..100_000).map(|i| i as f64 * 0.25).collect();
    let out: Vec<f64> = client
        .call_typed_slice_aligned("/scale2", &xs)
        .await
        .expect("aligned f64");
    assert_eq!(out, xs.iter().map(|x| x * 2.0).collect::<Vec<_>>());

    let fs: Vec<f32> = (0..1024).map(|i| i as f32 - 7.5).collect();
    let neg: Vec<f32> = client
        .call_typed_slice_aligned("/negate", &fs)
        .await
        .expect("aligned f32");
    assert_eq!(neg, fs.iter().map(|x| -x).collect::<Vec<_>>());

    // Empty slice must round-trip cleanly through the aligned form too.
    let empty: Vec<f64> = Vec::new();
    let back: Vec<f64> = client
        .call_typed_slice_aligned("/scale2", &empty)
        .await
        .expect("empty");
    assert!(back.is_empty());

    // Timeout-bearing variant succeeds on the happy path.
    let xs2: Vec<f64> = vec![1.0, 2.0, 3.0];
    let out2: Vec<f64> = client
        .call_typed_slice_aligned_with_timeout("/scale2", &xs2, Duration::from_secs(5))
        .await
        .expect("aligned with timeout");
    assert_eq!(out2, vec![2.0, 4.0, 6.0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_route_accepts_regular_typed_slice_client() {
    // A `_ref` route is a superset: a plain `call_typed_slice` (regular, unaligned
    // wire form) is bulk-copied into the borrowed `&[T]` and works unchanged.
    let router = Router::new().with_typed_slice_ref::<f64, f64, _>("/echo", |xs| Ok(xs.to_vec()));
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let xs: Vec<f64> = (0..256).map(|i| i as f64 * 1.5).collect();
    let out: Vec<f64> = client
        .call_typed_slice("/echo", &xs)
        .await
        .expect("regular -> ref");
    assert_eq!(out, xs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_route_accepts_serde_client() {
    // The serde path (`call_typed_beve` over `Vec<f64>`) also reaches a `_ref`
    // route, since its body is the regular typed array the superset decoder reads.
    let router = Router::new().with_typed_slice_ref::<f64, f64, _>("/echo", |xs| Ok(xs.to_vec()));
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let xs: Vec<f64> = vec![1.0, -2.5, 3.25, 4.0e9];
    let out: Vec<f64> = client
        .call_typed_beve("/echo", &xs)
        .await
        .expect("serde -> ref");
    assert_eq!(out, xs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aligned_call_against_regular_route_is_an_error() {
    // The aligned body is a distinct BEVE type; a plain `with_typed_slice` route
    // does not understand it and rejects rather than misreading.
    let router = Router::new().with_typed_slice::<f64, f64, _>("/echo", Ok);
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let xs: Vec<f64> = vec![1.0, 2.0, 3.0];
    let res: Result<Vec<f64>, _> = client.call_typed_slice_aligned("/echo", &xs).await;
    assert!(res.is_err(), "aligned body into a regular route must error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_element_type_is_an_error() {
    // Server expects f64; client sends an aligned f32 array. The element
    // class/width disagree, so the borrowing decoder rejects.
    let router = Router::new().with_typed_slice_ref::<f64, f64, _>("/echo", |xs| Ok(xs.to_vec()));
    let addr = spawn(router).await;
    let client = AsyncClient::connect(&addr).await.expect("connect");

    let fs: Vec<f32> = vec![1.0, 2.0, 3.0];
    let res: Result<Vec<f32>, _> = client.call_typed_slice_aligned("/echo", &fs).await;
    assert!(res.is_err(), "aligned f32 into an f64 route must error");
}

#[test]
fn sync_aligned_call_roundtrips_against_ref_route() {
    let router = Router::new().with_typed_slice_ref::<f64, f64, _>("/scale2", |xs| {
        Ok(xs.iter().map(|x| x * 2.0).collect())
    });
    let addr = spawn_sync(router);

    let client = Client::connect(&addr).expect("connect");
    let xs: Vec<f64> = (0..2048).map(|i| i as f64 * 0.5).collect();
    let out: Vec<f64> = client
        .call_typed_slice_aligned("/scale2", &xs)
        .expect("sync aligned");
    assert_eq!(out, xs.iter().map(|x| x * 2.0).collect::<Vec<_>>());

    let out2: Vec<f64> = client
        .call_typed_slice_aligned_with_timeout("/scale2", &xs, Duration::from_secs(5))
        .expect("sync aligned with timeout");
    assert_eq!(out2, xs.iter().map(|x| x * 2.0).collect::<Vec<_>>());
}
