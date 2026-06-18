//! End-to-end tests for the async SVS pull driver: a synchronous `Server`
//! producing a streamed value (it honours `Execution::OffReader`) and an
//! `AsyncClient` pulling it from async code, across the async/blocking decode
//! boundary, over a real TCP loopback connection. Covers the value, typed-slice,
//! and complex-slice receivers, each compressed and uncompressed.
#![cfg(feature = "value-stream")]

use repe::value_stream::{Compression, RouterValueStreamExt, StreamOpts};
use repe::{
    AsyncClient, Complex, RepeError, Router, Server, pull_complex_slice_async, pull_consume_async,
    pull_to_file_async, pull_to_file_verified_async, pull_typed_slice_async, pull_value_async,
};
use serde::{Deserialize, Serialize};
use std::io::{self, Cursor, Read};
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

// ---- opaque byte (reader) streams --------------------------------------------

/// ~1 MiB of a non-trivial byte pattern; spans many 16 KiB chunks so reassembly
/// and the async pull/decode bridge are exercised across many `next` round trips.
fn raw_blob() -> Vec<u8> {
    (0..1_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect()
}

/// A fresh, process-unique temp directory for a file-mode test.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("svs-async-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A router that streams `blob` verbatim (opaque bytes) for the key `"blob"`.
fn reader_router(blob: Vec<u8>, compression: Compression) -> Router {
    Router::new().with_reader_stream(
        move |resource: &str| (resource == "blob").then(|| Cursor::new(blob.clone())),
        small_chunks(compression),
    )
}

#[tokio::test]
async fn reader_stream_to_file_is_byte_identical() {
    let blob = raw_blob();
    let addr = spawn(reader_router(blob.clone(), Compression::None));
    let client = AsyncClient::connect(addr).await.expect("connect");

    let dir = temp_dir("raw-file");
    let path = dir.join("blob.bin");
    let n = pull_to_file_async(&client, "blob", &path)
        .await
        .expect("pull");
    assert_eq!(n as usize, blob.len());
    assert_eq!(std::fs::read(&path).unwrap(), blob);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn consume_async_reads_decompressed_content() {
    let blob = raw_blob();
    let addr = spawn(reader_router(blob.clone(), Compression::Zstd));
    let client = AsyncClient::connect(addr).await.expect("connect");

    let got: Vec<u8> = pull_consume_async(&client, "blob", |mut r: Box<dyn Read>| {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf)?;
        Ok(buf)
    })
    .await
    .expect("consume");
    assert_eq!(got, blob, "consume must see the decompressed content");
}

#[tokio::test]
async fn verified_pull_commits_when_the_digest_matches() {
    let blob = raw_blob();
    let addr = spawn(reader_router(blob.clone(), Compression::None));
    let client = AsyncClient::connect(addr).await.expect("connect");

    let dir = temp_dir("verify-ok");
    let path = dir.join("blob.bin");

    // `Vec<u8>` is the digest sink: it collects every teed content byte, so the
    // verify both checks integrity and proves the tee saw exactly the content.
    let expected = blob.clone();
    pull_to_file_verified_async(
        &client,
        "blob",
        &path,
        Vec::<u8>::new(),
        move |seen: Vec<u8>| {
            if seen == expected {
                Ok(())
            } else {
                Err(RepeError::Io(io::Error::other("digest mismatch")))
            }
        },
    )
    .await
    .expect("verified pull");
    assert_eq!(std::fs::read(&path).unwrap(), blob);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn verified_pull_commits_nothing_when_rejected() {
    let blob = raw_blob();
    let addr = spawn(reader_router(blob, Compression::None));
    let client = AsyncClient::connect(addr).await.expect("connect");

    let dir = temp_dir("verify-bad");
    let path = dir.join("blob.bin");

    let err = pull_to_file_verified_async(
        &client,
        "blob",
        &path,
        Vec::<u8>::new(),
        |_seen: Vec<u8>| Err(RepeError::Io(io::Error::other("rejected by test"))),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RepeError::Io(_)));
    assert!(!path.exists(), "a rejected verify must not commit the file");
    assert!(
        !dir.join("blob.bin.svspart").exists(),
        "the temp sibling must be cleaned up on rejection"
    );

    std::fs::remove_dir_all(&dir).ok();
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

    // An opaque byte stream pulled to a verified file over WebSocket: the exact
    // shape a remote save uses (server streams bytes it already holds, client
    // writes a byte-identical copy and gates the rename on a content digest).
    #[tokio::test]
    async fn verified_reader_stream_over_websocket() {
        let blob = raw_blob();
        let client = ws_client(reader_router(blob.clone(), Compression::None)).await;

        let dir = temp_dir("ws-verify");
        let path = dir.join("blob.bin");
        let expected = blob.clone();
        pull_to_file_verified_async(
            &client,
            "blob",
            &path,
            Vec::<u8>::new(),
            move |seen: Vec<u8>| {
                (seen == expected)
                    .then_some(())
                    .ok_or_else(|| RepeError::Io(io::Error::other("digest mismatch")))
            },
        )
        .await
        .expect("verified ws pull");
        assert_eq!(std::fs::read(&path).unwrap(), blob);

        std::fs::remove_dir_all(&dir).ok();
    }
}
