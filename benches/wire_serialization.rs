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
//!   reuses the body buffer and shifts the body back over the prefix in place,
//!   saving an allocation and a second buffer's worth of traffic.
//!
//! The fast path's win is not monotone in body size, and the sweep is sized to
//! show that. It wins by ~30% through 64 KiB and still wins at 256 KiB, loses
//! by up to ~19% between roughly 512 KiB and 8 MiB, then wins again by 16 MiB.
//! The penalty in that band is the overlapping backward `copy_within`, which
//! runs at ~50 GiB/s against ~58 GiB/s for a forward copy into fresh memory;
//! the win outside it is the allocation and page faults the in-place path
//! never pays, which grow until they dominate again. Aligning the shift to 64
//! bytes recovers only about a third of the gap, so the prefix length is not
//! worth padding. Deliberately left unthresholded: the band's edges are set by
//! cache size, allocator, and page-fault cost, so a size cutoff here would be
//! tuned to one machine. A multi-MiB body that cares should be framed with
//! `write_message_streaming` or `write_message_typed_slice`, which never
//! materialize a wire buffer at all.
//!
//! Body sizes span the small RPC response (64 B) through the multi-MiB
//! streaming chunk (`repe::stream` documents 64 MiB windows; bench up to a
//! representative 4 MiB chunk).
//!
//! A second group, `outbound_frame_beve`, asks whether the WebSocket server
//! should build a response frame straight from a serializable value instead of
//! serializing into a `Message` body first:
//!
//! * `via_message`: today's path -- `structio::to_beve(value)` into a tight buffer,
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

use benchit::{Bench, GroupResult, Throughput};
use repe::{BodyFormat, HEADER_SIZE, Header, Message, QueryFormat};

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

fn bench_wire_serialization(bench: &mut Bench) {
    // One group per body size, so the three framings are interleaved against
    // each other at a fixed size and the reported ratio answers the question
    // the module doc poses: parity for the slow path, a widening win for the
    // fast one. A single group spanning every size would ratio a 4 MiB case
    // against a 64 B one.
    for &size in BODY_SIZES {
        let mut group = bench.group(format!("wire_serialization/{size}"));
        group.throughput(Throughput::Bytes((HEADER_SIZE + QUERY.len() + size) as u64));

        // Build outside the timing loop; the cost being measured is the
        // serialization step, not the build.
        group.bench("to_vec", move |b| {
            b.iter_with(|| build_message_no_reserve(size), |msg| msg.to_vec())
        });
        group.bench("into_wire_bytes_slow", move |b| {
            b.iter_with(
                || build_message_no_reserve(size),
                |msg| msg.into_wire_bytes(),
            )
        });
        group.bench("into_wire_bytes_fast", move |b| {
            b.iter_with(
                || build_message_with_reserve(size),
                |msg| msg.into_wire_bytes(),
            )
        });
        group.finish();
    }
}

/// A handler-shaped response body: a small head plus a sized payload, encoded
/// as BEVE (the case `into_wire_bytes` cannot fast-path, since `beve::to_vec`
/// returns a tight buffer with no room for the wire prefix).
#[derive(Default, PartialEq, Debug)]
struct Payload {
    id: u64,
    data: Vec<u8>,
}
structio::object!(Payload { id, data });

/// Current outbound path: serialize the value to a tight BEVE `Vec`, wrap it in
/// a [`Message`], then frame it. Two allocations (body + wire) and one body
/// copy, because the tight body buffer has no room for the header+query prefix.
fn frame_via_message(value: &Payload) -> Vec<u8> {
    let body = structio::to_beve(value);
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
/// in behind it with `structio::beve::to_writer`, then back-patch the header
/// with the now-known body length. One allocation, one encode pass, no separate
/// body `Vec` and no body copy.
fn frame_direct(value: &Payload) -> Vec<u8> {
    let prefix_len = HEADER_SIZE + QUERY.len();
    let mut buf = Vec::with_capacity(prefix_len + 64);
    buf.resize(prefix_len, 0);
    structio::beve::to_writer(value, &mut buf).expect("a Vec sink cannot fail");
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

fn bench_outbound_frame_beve(bench: &mut Bench) {
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

    for &size in BODY_SIZES {
        let payload = Payload {
            id: 1,
            data: vec![0xAB; size],
        };
        let payload = &payload;

        let mut group = bench.group(format!("outbound_frame_beve/{size}"));
        group.throughput(Throughput::Bytes((HEADER_SIZE + QUERY.len() + size) as u64));
        group.bench("via_message", move |b| {
            b.iter(|| frame_via_message(payload))
        });
        group.bench("direct_frame", move |b| b.iter(|| frame_direct(payload)));
        group.finish();
    }
}

/// Framing a whole-body numeric `Vec<f64>`: the serde streaming path
/// (`serialized_size` + `to_writer_streaming`, two O(payload) element walks) vs
/// the typed-slice fast path (`write_message_typed_slice`: O(1) `typed_slice_size`
/// plus one bulk `to_writer_typed_slice`). Both emit identical wire bytes, so the
/// difference is exactly the two per-element traversals the bulk path skips.
fn bench_typed_numeric_framing(bench: &mut Bench) {
    const ELEM_COUNTS: &[usize] = &[64, 4096, 64 * 1024, 1024 * 1024];

    fn frame_serde(sink: &mut Vec<u8>, data: &[f64]) {
        sink.clear();
        let mut header = Header::new();
        header.query_format = QueryFormat::JsonPointer as u16;
        header.body_format = BodyFormat::Beve as u16;
        let body_len = structio::beve_size(&data) as u64;
        repe::write_message_streaming(sink, header, QUERY.as_bytes(), body_len, |w| {
            structio::beve::to_writer(&data, w)
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

    for &n in ELEM_COUNTS {
        let data: Vec<f64> = (0..n).map(|i| i as f64 * 0.5).collect();
        let data = &data;
        // Distinct sinks rather than one shared buffer: both cases are live at
        // once, so a single `&mut` would be borrowed twice.
        let mut serde_sink = Vec::new();
        let mut typed_sink = Vec::new();

        let mut group = bench.group(format!("typed_numeric_framing_f64/{n}"));
        group.throughput(Throughput::Bytes(
            (HEADER_SIZE + QUERY.len() + n * 8) as u64,
        ));
        group.bench("serde_stream", |bn| {
            bn.iter(|| {
                frame_serde(&mut serde_sink, data);
                serde_sink.len()
            })
        });
        group.bench("typed_slice", |bn| {
            bn.iter(|| {
                frame_typed(&mut typed_sink, data);
                typed_sink.len()
            })
        });
        // The one ratio here worth gating. It is algorithmic -- O(1)
        // `typed_slice_size` plus one bulk write, against two O(n) element
        // walks -- so it does not shrink on a slow or contended runner the way
        // a constant-factor margin would, and it widens with `n` rather than
        // narrowing. Measured at 0.031..0.038, so 0.2 is roughly 6x of slack:
        // it fires when the bulk path stops being taken at all, and not on
        // ordinary drift. The other groups' ratios are constant factors and
        // are deliberately left ungated.
        gate(
            &group.finish(),
            "typed_slice",
            0.2,
            "the bulk typed-slice path may have fallen back to per-element serde",
        );
    }
}

/// Fail the run if a within-run ratio invariant no longer holds.
///
/// Deliberately within-run rather than against a saved baseline: the ratio is
/// measured from interleaved rounds in this same process, so it carries no
/// stored state and does not care how loaded or how fast the machine is.
///
/// Skips rather than fails when the run did not actually measure the
/// comparison. An absent case means the group was filtered out; a `None` ratio
/// means this case *became* the reference because everything registered ahead
/// of it was filtered away, which is exactly what `cargo bench -- typed_slice`
/// does. Both are ordinary filtered runs, and failing on either would make
/// every narrow filter look like the regression this exists to catch.
fn gate(result: &GroupResult, case: &str, max_ratio: f64, suspect: &str) {
    let Some(measured) = result.cases.iter().find(|c| c.name == case) else {
        return;
    };
    let Some(ratio) = &measured.ratio else { return };
    assert!(
        ratio.point < max_ratio,
        "{}/{}: ratio {:.4} against the group reference exceeds the {max_ratio} gate; {suspect}",
        result.name,
        case,
        ratio.point,
    );
}

fn main() {
    let mut bench = Bench::from_args();
    bench_wire_serialization(&mut bench);
    bench_outbound_frame_beve(&mut bench);
    bench_typed_numeric_framing(&mut bench);
}
