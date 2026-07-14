//! End-to-end tests for the SVS download path: a synchronous `Server` producing
//! a streamed value and a `Client` pulling it into each of the three receiver
//! outputs, over a real TCP loopback connection.
#![cfg(feature = "value-stream")]

use repe::value_stream::{Compression, RouterValueStreamExt, StreamOpts, StreamOutput};
use repe::{
    BodyFormat, Client, RepeError, Router, Server, pull_stream, pull_to_beve_file, pull_to_file,
    pull_value,
};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Payload {
    id: u64,
    label: String,
    samples: Vec<f64>,
    tags: Vec<String>,
}

fn sample_payload() -> Payload {
    // Large enough (~1.6 MiB of f64 plus strings) to span many chunks once the
    // test forces a small chunk size, exercising the lookahead/last logic and
    // multi-chunk reassembly rather than a single-frame happy path.
    Payload {
        id: 0xFEED_BEEF,
        label: "serialized value stream".to_string(),
        samples: (0..200_000).map(|i| (i as f64) * 0.5 - 1234.5).collect(),
        tags: (0..64).map(|i| format!("tag-{i:03}")).collect(),
    }
}

/// Start a sync `Server` whose router streams `payload` for the resource key
/// `"payload"`, with the given options. Returns a connected client. The server
/// thread is detached (the process exits at test end).
fn serve(payload: Payload, opts: StreamOpts) -> Client {
    let router = Router::new().with_value_stream(
        move |resource: &str| (resource == "payload").then(|| payload.clone()),
        opts,
    );
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

#[test]
fn value_mode_roundtrips_compressed() {
    let payload = sample_payload();
    let client = serve(payload.clone(), small_chunks(Compression::Zstd));
    let got: Payload = pull_value(&client, "payload").expect("pull value");
    assert_eq!(got, payload);
}

#[test]
fn value_mode_roundtrips_uncompressed() {
    let payload = sample_payload();
    let client = serve(payload.clone(), small_chunks(Compression::None));
    let got: Payload = pull_value(&client, "payload").expect("pull value");
    assert_eq!(got, payload);
}

#[test]
fn beve_zst_file_decodes_back_to_the_value() {
    let payload = sample_payload();
    let client = serve(payload.clone(), small_chunks(Compression::Zstd));

    let dir = std::env::temp_dir().join(format!("svs-zst-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("value.beve.zst");

    pull_stream::<()>(&client, "payload", StreamOutput::BeveZstdFile(&path)).expect("pull file");

    // The file is the raw compressed stream: decompress, then BEVE-decode.
    let compressed = std::fs::read(&path).expect("read file");
    let beve_bytes = zstd::decode_all(&compressed[..]).expect("zstd decode");
    let decoded: Payload = beve::from_slice(&beve_bytes).expect("beve decode");
    assert_eq!(decoded, payload);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn beve_file_is_decompressed_and_decodes_back_to_the_value() {
    let payload = sample_payload();
    let client = serve(payload.clone(), small_chunks(Compression::Zstd));

    let dir = std::env::temp_dir().join(format!("svs-beve-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("value.beve");

    pull_to_beve_file(&client, "payload", &path).expect("pull decompressed file");

    // The file is already-decompressed BEVE: decode directly.
    let beve_bytes = std::fs::read(&path).expect("read file");
    let decoded: Payload = beve::from_slice(&beve_bytes).expect("beve decode");
    assert_eq!(decoded, payload);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_resource_errors() {
    let client = serve(sample_payload(), StreamOpts::default());
    let err = pull_value::<Payload>(&client, "does-not-exist").unwrap_err();
    match err {
        RepeError::ServerError { code, .. } => {
            assert_eq!(code, repe::ErrorCode::MethodNotFound);
        }
        other => panic!("expected a server MethodNotFound error, got {other:?}"),
    }
}

#[test]
fn output_incompatible_with_tags_is_rejected_before_writing() {
    // Server streams uncompressed; asking for a `.beve.zst` (raw-compressed)
    // output must be refused rather than mislabeling uncompressed bytes.
    let client = serve(sample_payload(), small_chunks(Compression::None));

    let dir = std::env::temp_dir().join(format!("svs-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("value.beve.zst");

    let err = pull_stream::<()>(&client, "payload", StreamOutput::BeveZstdFile(&path)).unwrap_err();
    assert!(
        matches!(err, RepeError::Io(_)),
        "expected a tag-mismatch error, got {err:?}"
    );
    // And nothing was committed at the final path.
    assert!(
        !path.exists(),
        "no file should be written on a tag mismatch"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn many_small_payloads_each_terminate_cleanly() {
    // Drive several sequential pulls over one connection to confirm a stream is
    // fully released (`last` reached, session removed) and the next one starts
    // clean — i.e. no leaked session state across transfers.
    let payload = Payload {
        id: 1,
        label: "tiny".to_string(),
        samples: vec![1.0, 2.0, 3.0],
        tags: vec!["x".to_string()],
    };
    let client = serve(payload.clone(), small_chunks(Compression::Zstd));
    for _ in 0..5 {
        let got: Payload = pull_value(&client, "payload").expect("pull");
        assert_eq!(got, payload);
    }
}

// ---- opaque byte (reader) streams --------------------------------------------

/// ~1 MiB of a non-trivial byte pattern, large enough to span many 16 KiB chunks
/// so reassembly and the lookahead/last logic are exercised; any truncation or
/// off-by-one would corrupt the verbatim compare.
fn raw_blob() -> Vec<u8> {
    (0..1_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect()
}

/// Start a sync `Server` whose router streams `blob` verbatim (opaque bytes) for
/// the resource key `"blob"`, with the given options. Returns a connected client.
fn serve_reader(blob: Vec<u8>, opts: StreamOpts) -> Client {
    let router = Router::new().with_reader_stream(
        move |resource: &str| (resource == "blob").then(|| Cursor::new(blob.clone())),
        opts,
    );
    let server = Server::new(router);
    let listener: TcpListener = server.listen("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let _ = server.serve(listener);
    });
    Client::connect(addr).expect("connect")
}

#[test]
fn reader_stream_writes_a_byte_identical_file_uncompressed() {
    let blob = raw_blob();
    let client = serve_reader(blob.clone(), small_chunks(Compression::None));

    let dir = std::env::temp_dir().join(format!("svs-raw-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("blob.bin");

    pull_to_file(&client, "blob", &path).expect("pull raw file");

    let got = std::fs::read(&path).expect("read file");
    assert_eq!(
        got, blob,
        "uncompressed reader stream must be byte-identical"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reader_stream_roundtrips_through_compression() {
    // Even tagged for zstd, the logical content the consumer writes must equal
    // the producer's source bytes (compression is transparent end to end).
    let blob = raw_blob();
    let client = serve_reader(blob.clone(), small_chunks(Compression::Zstd));

    let dir = std::env::temp_dir().join(format!("svs-raw-zstd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("blob.bin");

    pull_to_file(&client, "blob", &path).expect("pull raw file");

    let got = std::fs::read(&path).expect("read file");
    assert_eq!(
        got, blob,
        "decompressed content must equal the source bytes"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn value_puller_rejects_a_raw_binary_stream() {
    // A raw-binary stream is tagged `format != BEVE`; the value decoder must
    // refuse it up front rather than feed raw bytes to BEVE.
    let client = serve_reader(raw_blob(), small_chunks(Compression::None));
    let err = pull_value::<Payload>(&client, "blob").unwrap_err();
    assert!(
        matches!(err, RepeError::Io(_)),
        "expected a format-mismatch error, got {err:?}"
    );
}

#[test]
fn interrupted_file_pull_leaves_no_final_file() {
    // A receiver that never reaches `last` must not leave a file at the final
    // path. We simulate this by pointing the temp/commit logic at a directory we
    // can inspect and pulling a resource that does not exist (open fails before
    // any chunk), then asserting the final path is absent.
    let client = serve(sample_payload(), small_chunks(Compression::Zstd));
    let dir = std::env::temp_dir().join(format!("svs-int-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: &Path = &dir.join("nope.beve");

    let err = pull_to_beve_file(&client, "missing", path);
    assert!(err.is_err());
    assert!(
        !path.exists(),
        "final file must not exist after a failed pull"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---- app-written (writer) streams --------------------------------------------

/// Start a sync `Server` on TCP loopback serving `router`; returns a connected
/// client. The server thread is detached (the process exits at test end).
fn serve_router(router: Router) -> Client {
    let server = Server::new(router);
    let listener: TcpListener = server.listen("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let _ = server.serve(listener);
    });
    Client::connect(addr).expect("connect")
}

// A tiny non-cryptographic digest (FNV-1a, 64-bit) — enough to demonstrate the
// single-pass tee-and-verify seam without pulling in a hashing dependency.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// A `Write` that hashes the bytes it forwards (FNV-1a) — the app-side tee a
/// `with_writer_stream` closure uses to digest a value single-pass as it encodes.
struct HashTee<'a> {
    inner: &'a mut dyn Write,
    hash: u64,
}

impl<'a> HashTee<'a> {
    fn new(inner: &'a mut dyn Write) -> Self {
        Self {
            inner,
            hash: FNV_OFFSET,
        }
    }
    fn finish(self) -> u64 {
        self.hash
    }
}

impl Write for HashTee<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        for &b in &buf[..n] {
            self.hash ^= b as u64;
            self.hash = self.hash.wrapping_mul(FNV_PRIME);
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn writer_stream_with_beve_tag_pulls_as_a_value() {
    // A trailer-free write tagged BEVE is indistinguishable from with_value_stream
    // and decodes through the typed value puller.
    let payload = sample_payload();
    let expected = payload.clone();
    let router = Router::new().with_writer_stream(
        BodyFormat::Beve,
        move |resource: &str| {
            (resource == "payload").then(|| {
                let value = payload.clone();
                move |w: &mut dyn Write| -> std::io::Result<()> {
                    beve::to_writer_streaming(w, &value)
                        .map_err(|e| std::io::Error::other(e.to_string()))
                }
            })
        },
        small_chunks(Compression::Zstd),
    );
    let client = serve_router(router);
    let got: Payload = pull_value(&client, "payload").expect("pull value");
    assert_eq!(got, expected);
}

/// Serve `payload` as a single-pass `payload || digest` stream (a BEVE value plus
/// an 8-byte FNV-1a trailer over the logical bytes), tagged RawBinary.
fn serve_writer_digest(payload: Payload, opts: StreamOpts) -> Client {
    let router = Router::new().with_writer_stream(
        BodyFormat::RawBinary,
        move |resource: &str| {
            (resource == "payload").then(|| {
                let value = payload.clone();
                move |w: &mut dyn Write| -> std::io::Result<()> {
                    // Single pass: serialize the value into the sink while hashing
                    // the logical bytes, then append the digest as a trailer. No
                    // second serialization pass, no buffering of the whole value.
                    let mut tee = HashTee::new(w);
                    beve::to_writer_streaming(&mut tee, &value)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    let digest = tee.finish();
                    w.write_all(&digest.to_le_bytes())?;
                    Ok(())
                }
            })
        },
        opts,
    );
    serve_router(router)
}

/// Pull a `payload || digest` stream to a file, split the trailer, verify the
/// digest over the payload prefix, and BEVE-decode the prefix back to the value.
fn pull_split_and_verify(client: &Client, path: &Path) -> Payload {
    pull_to_file(client, "payload", path).expect("pull");
    let bytes = std::fs::read(path).expect("read file");
    assert!(
        bytes.len() > 8,
        "stream must carry payload plus an 8-byte trailer"
    );
    let (payload_bytes, digest_bytes) = bytes.split_at(bytes.len() - 8);
    let got_digest = u64::from_le_bytes(digest_bytes.try_into().unwrap());
    assert_eq!(
        fnv1a(payload_bytes),
        got_digest,
        "trailer digest must match the streamed payload (end-to-end integrity)"
    );
    beve::from_slice(payload_bytes).expect("beve decode of payload prefix")
}

#[test]
fn writer_stream_single_pass_digest_roundtrips_uncompressed() {
    let payload = sample_payload();
    let client = serve_writer_digest(payload.clone(), small_chunks(Compression::None));

    let dir = std::env::temp_dir().join(format!("svs-wdig-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("payload.beve.fnv");

    let decoded = pull_split_and_verify(&client, &path);
    assert_eq!(decoded, payload);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn writer_stream_single_pass_digest_roundtrips_through_compression() {
    // The digest covers the logical (pre-compression) bytes; zstd is transparent
    // end to end, so the consumer recovers the same payload || digest and the
    // check still holds.
    let payload = sample_payload();
    let client = serve_writer_digest(payload.clone(), small_chunks(Compression::Zstd));

    let dir = std::env::temp_dir().join(format!("svs-wdig-zstd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("payload.beve.fnv");

    let decoded = pull_split_and_verify(&client, &path);
    assert_eq!(decoded, payload);

    std::fs::remove_dir_all(&dir).ok();
}
