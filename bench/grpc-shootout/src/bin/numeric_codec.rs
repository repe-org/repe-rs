//! Isolates the serde-BEVE vs bulk typed-slice gap for contiguous f32/f64.
//!
//! This is the root-cause measurement behind the e2e finding that large `f64`
//! arrays lose to gRPC on the high-level REPE API: that API encodes/decodes
//! numeric `Vec`s through serde (per-element), while `to_writer_typed_slice` /
//! `read_typed_slice` move the same bytes in one bulk pass. Both produce
//! byte-identical wire output, so the delta here is exactly the per-element
//! traversal the bulk path skips -- the ceiling on what wiring the fast path
//! into the high-level API would recover.
//!
//!   cargo run --release --bin numeric_codec

use std::time::Instant;

const COUNTS: &[usize] = &[64, 4_096, 65_536, 1_048_576];

fn time<F: FnMut()>(iters: usize, mut f: F) -> f64 {
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_secs_f64() / iters as f64 * 1e9 // ns/op
}

fn iters_for(n: usize) -> usize {
    // Keep total work roughly constant across sizes.
    (8_000_000 / n).clamp(20, 200_000)
}

macro_rules! sweep {
    ($ty:ty, $label:literal, $mk:expr) => {{
        println!("\n== {} ==", $label);
        println!(
            "{:>9}  {:>12} {:>12} {:>6}   {:>12} {:>12} {:>6}",
            "n", "serde_enc", "bulk_enc", "x", "serde_dec", "bulk_dec", "x"
        );
        for &n in COUNTS {
            let data: Vec<$ty> = (0..n).map($mk).collect();
            let iters = iters_for(n);

            // Guard: serde and bulk encodings must be byte-identical.
            let serde_bytes = beve::to_vec(&data).unwrap();
            let bulk_bytes = beve::to_vec_typed_slice(&data);
            assert_eq!(serde_bytes, bulk_bytes, "{} n={}: wire mismatch", $label, n);

            let mut sink = Vec::with_capacity(bulk_bytes.len());
            let serde_enc = time(iters, || {
                sink.clear();
                beve::to_writer_streaming(&mut sink, &data).unwrap();
            });
            let bulk_enc = time(iters, || {
                sink.clear();
                beve::to_writer_typed_slice(&mut sink, &data).unwrap();
            });
            let serde_dec = time(iters, || {
                let v: Vec<$ty> = beve::from_slice(&bulk_bytes).unwrap();
                std::hint::black_box(v);
            });
            let bulk_dec = time(iters, || {
                let v: Vec<$ty> = beve::read_typed_slice(&bulk_bytes).unwrap();
                std::hint::black_box(v);
            });

            println!(
                "{:>9}  {:>10.0}ns {:>10.0}ns {:>5.1}x   {:>10.0}ns {:>10.0}ns {:>5.1}x",
                n,
                serde_enc,
                bulk_enc,
                serde_enc / bulk_enc,
                serde_dec,
                bulk_dec,
                serde_dec / bulk_dec,
            );
        }
    }};
}

fn main() {
    sweep!(f64, "f64", |i| i as f64 * 0.5);
    sweep!(f32, "f32", |i| i as f32 * 0.5);
}
