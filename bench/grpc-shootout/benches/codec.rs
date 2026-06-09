//! Codec-only shootout: serialization + framing CPU, no network.
//!
//! This isolates the question "which encoding turns a value into wire bytes (and
//! back) faster" from all transport concerns. Each shape is measured four ways:
//! protobuf encode, REPE+BEVE encode/frame, protobuf decode, REPE+BEVE parse.
//!
//! REPE numeric bodies use the bulk typed-slice fast path
//! (`write_message_typed_slice` / `Message::decode_typed_slice`): on
//! little-endian targets the body is one memcpy, O(1) in the element count.
//! protobuf walks every element (fixed64 copy for `double`, varint decode for
//! `sint64`).
//!
//! Each group prints the wire size via `Throughput::Bytes`, so the report also
//! shows how much smaller (or larger) each encoding is on the wire -- relevant
//! because bytes-on-wire feeds directly into the e2e transport cost.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use grpc_shootout::{ELEM_COUNTS, Small, pb, sample_f64, sample_i64};
use prost::Message as _;
use repe::{Header, Message, write_message_typed_slice};
use std::hint::black_box;

const QUERY: &str = "/echo";

// ---- f64: protobuf fixed64 vs BEVE bulk typed slice -----------------------

fn bench_f64(c: &mut Criterion) {
    let mut enc = c.benchmark_group("f64_encode");
    for &n in ELEM_COUNTS {
        let data = sample_f64(n);
        let pb_msg = pb::NumericF64 { values: data.clone() };

        // Guard: both encodings must round-trip to the same logical value.
        let mut repe_buf = Vec::new();
        write_message_typed_slice(&mut repe_buf, Header::new(), QUERY.as_bytes(), &data).unwrap();
        let back: Vec<f64> = Message::from_slice(&repe_buf).unwrap().decode_typed_slice().unwrap();
        assert_eq!(back, data);
        assert_eq!(pb::NumericF64::decode(pb_msg.encode_to_vec().as_slice()).unwrap().values, data);

        enc.throughput(Throughput::Bytes((n * 8) as u64));
        enc.bench_with_input(BenchmarkId::new("protobuf", n), &pb_msg, |b, m| {
            b.iter(|| black_box(m.encode_to_vec()));
        });
        enc.bench_with_input(BenchmarkId::new("repe_beve", n), &data, |b, d| {
            let mut sink = Vec::new();
            b.iter(|| {
                sink.clear();
                write_message_typed_slice(&mut sink, Header::new(), QUERY.as_bytes(), d).unwrap();
                black_box(&sink);
            });
        });
    }
    enc.finish();

    let mut dec = c.benchmark_group("f64_decode");
    for &n in ELEM_COUNTS {
        let data = sample_f64(n);
        let pb_wire = pb::NumericF64 { values: data.clone() }.encode_to_vec();
        let mut repe_wire = Vec::new();
        write_message_typed_slice(&mut repe_wire, Header::new(), QUERY.as_bytes(), &data).unwrap();

        dec.throughput(Throughput::Bytes((n * 8) as u64));
        dec.bench_with_input(BenchmarkId::new("protobuf", n), &pb_wire, |b, w| {
            b.iter(|| black_box(pb::NumericF64::decode(w.as_slice()).unwrap()));
        });
        dec.bench_with_input(BenchmarkId::new("repe_beve", n), &repe_wire, |b, w| {
            b.iter(|| {
                let msg = Message::from_slice(w).unwrap();
                let v: Vec<f64> = msg.decode_typed_slice().unwrap();
                black_box(v);
            });
        });
    }
    dec.finish();
}

// ---- sint64: protobuf zig-zag varint vs BEVE bulk typed slice -------------
//
// protobuf's worst numeric case; expect the widest gap here.

fn bench_i64(c: &mut Criterion) {
    let mut enc = c.benchmark_group("i64_encode");
    for &n in ELEM_COUNTS {
        let data = sample_i64(n);
        let pb_msg = pb::NumericI64 { values: data.clone() };

        let mut repe_buf = Vec::new();
        write_message_typed_slice(&mut repe_buf, Header::new(), QUERY.as_bytes(), &data).unwrap();
        let back: Vec<i64> = Message::from_slice(&repe_buf).unwrap().decode_typed_slice().unwrap();
        assert_eq!(back, data);

        enc.throughput(Throughput::Bytes((n * 8) as u64));
        enc.bench_with_input(BenchmarkId::new("protobuf", n), &pb_msg, |b, m| {
            b.iter(|| black_box(m.encode_to_vec()));
        });
        enc.bench_with_input(BenchmarkId::new("repe_beve", n), &data, |b, d| {
            let mut sink = Vec::new();
            b.iter(|| {
                sink.clear();
                write_message_typed_slice(&mut sink, Header::new(), QUERY.as_bytes(), d).unwrap();
                black_box(&sink);
            });
        });
    }
    enc.finish();

    let mut dec = c.benchmark_group("i64_decode");
    for &n in ELEM_COUNTS {
        let data = sample_i64(n);
        let pb_wire = pb::NumericI64 { values: data.clone() }.encode_to_vec();
        let mut repe_wire = Vec::new();
        write_message_typed_slice(&mut repe_wire, Header::new(), QUERY.as_bytes(), &data).unwrap();

        dec.throughput(Throughput::Bytes((n * 8) as u64));
        dec.bench_with_input(BenchmarkId::new("protobuf", n), &pb_wire, |b, w| {
            b.iter(|| black_box(pb::NumericI64::decode(w.as_slice()).unwrap()));
        });
        dec.bench_with_input(BenchmarkId::new("repe_beve", n), &repe_wire, |b, w| {
            b.iter(|| {
                let msg = Message::from_slice(w).unwrap();
                let v: Vec<i64> = msg.decode_typed_slice().unwrap();
                black_box(v);
            });
        });
    }
    dec.finish();
}

// ---- Small struct: the narrow-gap case -----------------------------------

fn bench_small(c: &mut Criterion) {
    let small = Small::sample();
    let pb_small = pb::Small {
        id: small.id,
        name: small.name.clone(),
        ok: small.ok,
        score: small.score,
    };

    let mut g = c.benchmark_group("small_roundtrip");
    g.bench_function("protobuf", |b| {
        b.iter(|| {
            let wire = pb_small.encode_to_vec();
            black_box(pb::Small::decode(wire.as_slice()).unwrap());
        });
    });
    g.bench_function("repe_beve", |b| {
        b.iter(|| {
            let wire = Message::builder()
                .query_str(QUERY)
                .body_beve(&small)
                .unwrap()
                .build()
                .into_wire_bytes();
            let got: Small = Message::from_slice(&wire).unwrap().beve_body().unwrap();
            black_box(got);
        });
    });
    g.finish();
}

criterion_group!(benches, bench_f64, bench_i64, bench_small);
criterion_main!(benches);
