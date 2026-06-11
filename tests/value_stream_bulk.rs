//! End-to-end tests for the SVS bulk path: a `with_typed_value_stream` /
//! `with_complex_value_stream` producer streaming a numeric or complex array, and
//! a client decoding it at bulk speed with `pull_typed_slice` / `pull_complex_slice`.
#![cfg(feature = "value-stream")]

use repe::value_stream::{Compression, RouterValueStreamExt, StreamOpts};
use repe::{
    Client, Complex, RepeError, Router, Server, pull_complex_slice, pull_to_beve_zst_file,
    pull_typed_slice,
};
use std::net::TcpListener;
use std::thread;

fn serve(router: Router) -> Client {
    let server = Server::new(router);
    let listener: TcpListener = server.listen("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let _ = server.serve(listener);
    });
    Client::connect(addr).expect("connect")
}

fn small_chunks(compression: Compression) -> StreamOpts {
    StreamOpts {
        chunk_bytes: 16 * 1024,
        compression,
        zstd_level: 3,
        session_depth: 4,
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

fn typed_router(values: Vec<f64>, opts: StreamOpts) -> Router {
    Router::new().with_typed_value_stream(
        move |resource: &str| (resource == "buf").then(|| values.clone()),
        opts,
    )
}

fn complex_router(values: Vec<Complex<f32>>, opts: StreamOpts) -> Router {
    Router::new().with_complex_value_stream(
        move |resource: &str| (resource == "iq").then(|| values.clone()),
        opts,
    )
}

#[test]
fn typed_slice_roundtrips_uncompressed() {
    let v = f64_samples();
    let client = serve(typed_router(v.clone(), small_chunks(Compression::None)));
    let got: Vec<f64> = pull_typed_slice(&client, "buf").expect("pull typed");
    assert_eq!(got, v);
}

#[test]
fn typed_slice_roundtrips_compressed() {
    let v = f64_samples();
    let client = serve(typed_router(v.clone(), small_chunks(Compression::Zstd)));
    let got: Vec<f64> = pull_typed_slice(&client, "buf").expect("pull typed");
    assert_eq!(got, v);
}

#[test]
fn complex_slice_roundtrips_uncompressed() {
    let v = iq_samples();
    let client = serve(complex_router(v.clone(), small_chunks(Compression::None)));
    let got: Vec<Complex<f32>> = pull_complex_slice(&client, "iq").expect("pull complex");
    assert_eq!(got, v);
}

#[test]
fn complex_slice_roundtrips_compressed() {
    let v = iq_samples();
    let client = serve(complex_router(v.clone(), small_chunks(Compression::Zstd)));
    let got: Vec<Complex<f32>> = pull_complex_slice(&client, "iq").expect("pull complex");
    assert_eq!(got, v);
}

#[test]
fn empty_typed_slice_roundtrips() {
    let client = serve(typed_router(Vec::new(), small_chunks(Compression::Zstd)));
    let got: Vec<f64> = pull_typed_slice(&client, "buf").expect("pull empty typed");
    assert!(got.is_empty());
}

#[test]
fn wrong_element_type_is_rejected() {
    // The stream is an f64 typed array; pulling it as i32 must error (the bulk
    // reader validates the element class/width against the wire header) rather
    // than silently misreading.
    let client = serve(typed_router(f64_samples(), small_chunks(Compression::None)));
    let err = pull_typed_slice::<i32>(&client, "buf").unwrap_err();
    assert!(
        matches!(err, RepeError::Beve(_)),
        "expected a BEVE type mismatch, got {err:?}"
    );
}

#[test]
fn typed_producer_bytes_are_also_a_valid_beve_zst_file() {
    // A typed producer emits ordinary BEVE on the wire, so the format-agnostic
    // file output works too: pull to a .beve.zst, decompress + bulk-decode, and
    // confirm it matches the source.
    let v = f64_samples();
    let client = serve(typed_router(v.clone(), small_chunks(Compression::Zstd)));

    let dir = std::env::temp_dir().join(format!("svs-typed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("buf.beve.zst");

    pull_to_beve_zst_file(&client, "buf", &path).expect("pull file");

    let compressed = std::fs::read(&path).unwrap();
    let beve_bytes = zstd::decode_all(&compressed[..]).unwrap();
    let decoded = beve::read_typed_slice::<f64>(&beve_bytes).unwrap();
    assert_eq!(decoded, v);

    std::fs::remove_dir_all(&dir).ok();
}
