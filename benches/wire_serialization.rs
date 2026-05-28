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

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use repe::{BodyFormat, HEADER_SIZE, Message, QueryFormat};
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
        group.throughput(Throughput::Bytes(
            (HEADER_SIZE + QUERY.len() + size) as u64,
        ));

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

criterion_group!(benches, bench_wire_serialization);
criterion_main!(benches);
