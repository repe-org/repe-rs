//! Outbound wire-serialization throughput.
//!
//! Measures the cost of turning a [`Message`] into a `Vec<u8>` suitable for
//! the WebSocket writer sink. Three shapes are compared:
//!
//! * `to_vec`: the original API; always allocates a fresh wire buffer and
//!   copies the body into it.
//! * `into_wire_bytes` (slow path): same input shape as `to_vec`, but the
//!   message is consumed. Body has no spare capacity for the prefix, so the
//!   method falls back to a fresh allocation. Should be at parity with
//!   `to_vec`.
//! * `into_wire_bytes` (fast path): the body is pre-allocated with
//!   `Vec::with_capacity(body_len + HEADER_SIZE + query.len())`. The method
//!   reuses the body buffer and performs one in-place memcpy. Should improve
//!   noticeably as the body grows.
//!
//! Body sizes span the small RPC response (64 B) through the multi-MiB
//! streaming chunk (`repe::stream` documents 64 MiB windows; bench up to a
//! representative 4 MiB chunk).
//!
//! A second group, `outbound_frame_beve`, asks whether the WebSocket server
//! should build a response frame straight from a serializable value instead of
//! serializing into a `Message` body first:
//!
//! * `via_message`: today's path -- `beve::to_vec(value)` into a tight buffer,
//!   wrap in a `Message`, then `into_wire_bytes`. The BEVE buffer is tight
//!   (cap == len), so the prefix never fits and `into_wire_bytes` falls to its
//!   slow path: a second allocation plus a body copy.
//! * `direct_frame`: reserve the header+query prefix in one buffer, stream the
//!   body in behind it with `beve::to_writer_streaming`, then back-patch the
//!   header with the now-known body length. One allocation, no body copy.
//!
//! Finding: the BEVE encode dominates and is identical in both paths, so
//! direct-framing removes only the framing overhead -- a large relative win for
//! tiny, allocation-bound bodies (~40% at 64 B) but single digits once the body
//! grows (~4-8% from 4 KiB up). Crucially, the server's handlers return
//! `serde_json::Value`, which BEVE encodes element-by-element (there is no
//! bulk-bytes fast path to make the encode O(1)), so its real responses are
//! encode-bound and the framing win is marginal. That does not justify
//! reworking the outbound `Message` channel into a lazy-serialize-at-the-writer
//! representation -- which would also have to keep encoding on the per-handler
//! threads to avoid serializing the writer task. The earlier "~60%" estimate
//! reflected a bulk-byte body (O(1) encode, copy-bound), which is not the shape
//! the server actually produces.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use repe::{BodyFormat, HEADER_SIZE, Header, Message, QueryFormat};
use serde::{Deserialize, Serialize};
use std::hint::black_box;

const QUERY: &str = "/collect/file_chunk";
const BODY_SIZES: &[usize] = &[64, 4 * 1024, 64 * 1024, 4 * 1024 * 1024];

fn build_message_no_reserve(body_len: usize) -> Message {
    Message::builder()
        .id(1)
        .query_str(QUERY)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(vec![0xABu8; body_len])
        .body_format(BodyFormat::RawBinary)
        .build()
}

fn build_message_with_reserve(body_len: usize) -> Message {
    let mut body = Vec::with_capacity(HEADER_SIZE + QUERY.len() + body_len);
    body.resize(body_len, 0xAB);
    Message::builder()
        .id(1)
        .query_str(QUERY)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(body)
        .body_format(BodyFormat::RawBinary)
        .build()
}

fn bench_wire_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("wire_serialization");
    for &size in BODY_SIZES {
        group.throughput(Throughput::Bytes((HEADER_SIZE + QUERY.len() + size) as u64));

        group.bench_with_input(BenchmarkId::new("to_vec", size), &size, |b, &size| {
            // Build outside the timing loop; the cost being measured is the
            // serialization step, not the build.
            b.iter_batched(
                || build_message_no_reserve(size),
                |msg| black_box(msg.to_vec()),
                criterion::BatchSize::SmallInput,
            );
        });

        group.bench_with_input(
            BenchmarkId::new("into_wire_bytes_slow", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || build_message_no_reserve(size),
                    |msg| black_box(msg.into_wire_bytes()),
                    criterion::BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("into_wire_bytes_fast", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || build_message_with_reserve(size),
                    |msg| black_box(msg.into_wire_bytes()),
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

/// A handler-shaped response body: a small head plus a sized payload, encoded
/// as BEVE (the case `into_wire_bytes` cannot fast-path, since `beve::to_vec`
/// returns a tight buffer with no room for the wire prefix).
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Payload {
    id: u64,
    data: Vec<u8>,
}

/// Current outbound path: serialize the value to a tight BEVE `Vec`, wrap it in
/// a [`Message`], then frame it. Two allocations (body + wire) and one body
/// copy, because the tight body buffer has no room for the header+query prefix.
fn frame_via_message(value: &Payload) -> Vec<u8> {
    let body = beve::to_vec(value).unwrap();
    Message::builder()
        .id(value.id)
        .query_str(QUERY)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(body)
        .body_format(BodyFormat::Beve)
        .build()
        .into_wire_bytes()
}

/// Direct-frame: reserve the header+query prefix in one buffer, stream the body
/// in behind it with `beve::to_writer_streaming`, then back-patch the header
/// with the now-known body length. One allocation, one encode pass, no separate
/// body `Vec` and no body copy.
fn frame_direct(value: &Payload) -> Vec<u8> {
    let prefix_len = HEADER_SIZE + QUERY.len();
    let mut buf = Vec::with_capacity(prefix_len + 64);
    buf.resize(prefix_len, 0);
    beve::to_writer_streaming(&mut buf, value).unwrap();
    let body_len = (buf.len() - prefix_len) as u64;

    let mut header = Header::new();
    header.id = value.id;
    header.query_format = QueryFormat::JsonPointer as u16;
    header.body_format = BodyFormat::Beve as u16;
    header.query_length = QUERY.len() as u64;
    header.body_length = body_len;
    header.length = buf.len() as u64;
    buf[..HEADER_SIZE].copy_from_slice(&header.encode());
    buf[HEADER_SIZE..prefix_len].copy_from_slice(QUERY.as_bytes());
    buf
}

fn bench_outbound_frame_beve(c: &mut Criterion) {
    // Guard: both framings must round-trip to the same value, so the benchmark
    // compares equivalent work rather than a shortcut.
    let probe = Payload {
        id: 9,
        data: vec![1, 2, 3, 4, 5],
    };
    let via = Message::from_slice(&frame_via_message(&probe)).unwrap();
    let direct = Message::from_slice(&frame_direct(&probe)).unwrap();
    assert_eq!(via.beve_body::<Payload>().unwrap(), probe);
    assert_eq!(direct.beve_body::<Payload>().unwrap(), probe);

    let mut group = c.benchmark_group("outbound_frame_beve");
    for &size in BODY_SIZES {
        let payload = Payload {
            id: 1,
            data: vec![0xAB; size],
        };
        group.throughput(Throughput::Bytes((HEADER_SIZE + QUERY.len() + size) as u64));

        group.bench_with_input(BenchmarkId::new("via_message", size), &payload, |b, p| {
            b.iter(|| black_box(frame_via_message(p)));
        });
        group.bench_with_input(BenchmarkId::new("direct_frame", size), &payload, |b, p| {
            b.iter(|| black_box(frame_direct(p)));
        });
    }
    group.finish();
}

/// Framing a whole-body numeric `Vec<f64>`: the serde streaming path
/// (`serialized_size` + `to_writer_streaming`, two O(payload) element walks) vs
/// the typed-slice fast path (`write_message_typed_slice`: O(1) `typed_slice_size`
/// plus one bulk `to_writer_typed_slice`). Both emit identical wire bytes, so the
/// difference is exactly the two per-element traversals the bulk path skips.
fn bench_typed_numeric_framing(c: &mut Criterion) {
    const ELEM_COUNTS: &[usize] = &[64, 4096, 64 * 1024, 1024 * 1024];

    fn frame_serde(sink: &mut Vec<u8>, data: &[f64]) {
        sink.clear();
        let mut header = Header::new();
        header.query_format = QueryFormat::JsonPointer as u16;
        header.body_format = BodyFormat::Beve as u16;
        let body_len = beve::serialized_size(&data).unwrap();
        repe::write_message_streaming(sink, header, QUERY.as_bytes(), body_len, |w| {
            beve::to_writer_streaming(w, &data)
        })
        .unwrap();
    }
    fn frame_typed(sink: &mut Vec<u8>, data: &[f64]) {
        sink.clear();
        let mut header = Header::new();
        header.query_format = QueryFormat::JsonPointer as u16;
        repe::write_message_typed_slice(sink, header, QUERY.as_bytes(), data).unwrap();
    }

    // Guard: both framings must produce identical wire bytes, so the benchmark
    // compares equivalent work rather than a shortcut.
    let probe: Vec<f64> = (0..100).map(|i| i as f64 * 0.5).collect();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    frame_serde(&mut a, &probe);
    frame_typed(&mut b, &probe);
    assert_eq!(
        a, b,
        "serde and typed-slice framings must agree byte-for-byte"
    );

    let mut group = c.benchmark_group("typed_numeric_framing_f64");
    for &n in ELEM_COUNTS {
        let data: Vec<f64> = (0..n).map(|i| i as f64 * 0.5).collect();
        group.throughput(Throughput::Bytes(
            (HEADER_SIZE + QUERY.len() + n * 8) as u64,
        ));
        let mut serde_sink = Vec::new();
        let mut typed_sink = Vec::new();

        group.bench_with_input(BenchmarkId::new("serde_stream", n), &data, |bn, d| {
            bn.iter(|| {
                frame_serde(&mut serde_sink, d);
                black_box(&serde_sink);
            });
        });
        group.bench_with_input(BenchmarkId::new("typed_slice", n), &data, |bn, d| {
            bn.iter(|| {
                frame_typed(&mut typed_sink, d);
                black_box(&typed_sink);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_wire_serialization,
    bench_outbound_frame_beve,
    bench_typed_numeric_framing
);
criterion_main!(benches);
