# High-Throughput Numeric Bodies

When a whole REPE message body is a contiguous numeric slice -- `&[f64]`, `&[i32]`, a `&[Complex<f64>]`, a matrix of samples -- repe can encode and decode it as a BEVE typed array in a single bulk copy, bypassing serde's element-by-element walk. On little-endian targets the encode, decode, and framing are O(1) in the element count (a header plus one `memcpy`), versus the per-element traversal a serde body pays.

This is opt-in and applies to a **whole-body** numeric slice. A numeric `Vec<T>` nested as a field inside a larger struct still goes through serde (serde never exposes the field's backing slice), so reach for it when the entire body is the array.

## Encoding

`MessageBuilder::body_typed_slice` and `body_complex_slice` encode a slice in one bulk write and set `BodyFormat::Beve`:

```rust
use repe::{Complex, Message};

let samples: Vec<f64> = (0..4096).map(|i| (i as f64 * 0.25).sin()).collect();
let msg = Message::builder()
    .query_str("/spectra/ingest")
    .body_typed_slice(&samples)
    .build();

let spectrum: Vec<Complex<f64>> = /* ... */;
let cmsg = Message::builder()
    .body_complex_slice(&spectrum)
    .build();
```

The bytes are **identical** to `body_beve(&samples)` (the serde path), so a bulk sender and a serde receiver -- or the reverse -- interoperate freely. The difference is only the cost of producing them.

## Decoding

`Message::decode_typed_slice` / `decode_complex_slice` read the body back in one bounds-checked bulk copy:

```rust
let back: Vec<f64> = msg.decode_typed_slice()?;
let cback: Vec<Complex<f64>> = cmsg.decode_complex_slice()?;
```

They error with `RepeError::UnexpectedBodyFormat` if the body is not `BodyFormat::Beve`, and `RepeError::Beve` if the bytes are not a typed array of the requested element type (wrong class/width, or a truncated payload) -- never a silent reinterpretation. Because the bytes match the serde encoding, `decode_typed_slice` also reads a body that was produced by `body_beve(&Vec<T>)`, and conversely `beve_body::<Vec<T>>()` reads a `body_typed_slice` body.

## Streaming a large body with no body buffer

For a body too large to hold a second copy of, `write_message_typed_slice` sizes the body in closed form (`beve::typed_slice_size`, no traversal) and writes the payload straight to the sink:

```rust
use repe::{Header, write_message_typed_slice};

let big: Vec<f64> = /* millions of samples */;
write_message_typed_slice(&mut sink, Header::new(), b"/spectra/stream", &big)?;
```

The wire frame is byte-for-byte identical to building a `body_typed_slice` message and writing it with `write_message`; it just never materializes the body or a wire-frame `Vec`. (There is no streaming writer for complex bodies yet -- build a `Message` with `body_complex_slice` and frame it with `write_message`.)

## Performance

Framing a whole-body numeric `Vec<f64>`, serde streaming (`serialized_size` + `to_writer_streaming`, two O(payload) element walks) vs the typed-slice fast path (O(1) size + one bulk write), from `benches/wire_serialization.rs`:

| elements | serde stream | typed slice | speedup |
|---:|---:|---:|---:|
| 64 | 268 ns | 19 ns | ~14x |
| 4 096 | 17.6 us | 535 ns | ~33x |
| 65 536 | 281 us | 8.9 us | ~31x |
| 1 048 576 | 4.44 ms | 168 us | ~26x |

The win grows with body size until it is bound by memory bandwidth (the bulk copy), where it plateaus around 25-33x.

See `examples/typed_numeric_body.rs` for a runnable end-to-end demo.
