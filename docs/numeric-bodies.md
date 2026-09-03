# High-Throughput Numeric Bodies

When a whole REPE message body is a contiguous numeric slice -- `&[f64]`, `&[i32]`, a `&[Complex<f64>]`, a matrix of samples -- repe can encode and decode it as a BEVE typed array in a single bulk copy, in a single bulk copy rather than an element-by-element walk. On little-endian targets the encode, decode, and framing are O(1) in the element count (a header plus one `memcpy`).

This is opt-in and applies to a **whole-body** numeric slice. A numeric `Vec<T>` nested as a field inside a larger struct is written by the same writer and takes the same bulk path for its own payload; what these APIs add is measuring the length in closed form so the frame's buffer is allocated once.

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

The bytes are **identical** to `body_beve(&samples)` -- structio has one writer, and it already takes the bulk path for a numeric vector -- so a bulk sender and an ordinary receiver, or the reverse, interoperate freely. What these APIs add is the closed-form size, so the buffer is allocated once rather than grown.

## Decoding

`Message::decode_typed_slice` / `decode_complex_slice` read the body back in one bounds-checked bulk copy:

```rust
let back: Vec<f64> = msg.decode_typed_slice()?;
let cback: Vec<Complex<f64>> = cmsg.decode_complex_slice()?;
```

They error with `RepeError::UnexpectedBodyFormat` if the body is not `BodyFormat::Beve`, and `RepeError::Beve` if the bytes are not a numeric array at all, or hold an integer that does not fit the requested width -- never a silent reinterpretation. A *width* mismatch is not an error: a stored `f64` array read as `f32` is converted element by element, and the bulk path is taken wherever the widths agree. `decode_typed_slice` and `beve_body::<Vec<T>>()` read each other's bodies, because there is one reader underneath both.

## Streaming a large body with no body buffer

For a body too large to hold a second copy of, `write_message_typed_slice` sizes the body in closed form (`structio::beve_size`, no traversal) and writes the payload straight to the sink:

```rust
use repe::{Header, write_message_typed_slice};

let big: Vec<f64> = /* millions of samples */;
write_message_typed_slice(&mut sink, Header::new(), b"/spectra/stream", &big)?;
```

The wire frame is byte-for-byte identical to building a `body_typed_slice` message and writing it with `write_message`; it just never materializes the body or a wire-frame `Vec`. (There is no streaming writer for complex bodies yet -- build a `Message` with `body_complex_slice` and frame it with `write_message`.)

## Performance

Framing a whole-body numeric `Vec<f64>`, a size pass plus a stream write vs the typed-slice fast path (O(1) size + one bulk write), from `benches/wire_serialization.rs`:

| elements | size + stream | typed slice | speedup |
|---:|---:|---:|---:|
| 64 | 268 ns | 19 ns | ~14x |
| 4 096 | 17.6 us | 535 ns | ~33x |
| 65 536 | 281 us | 8.9 us | ~31x |
| 1 048 576 | 4.44 ms | 168 us | ~26x |

The win grows with body size until it is bound by memory bandwidth (the bulk copy), where it plateaus around 25-33x.

See `examples/typed_numeric_body.rs` for a runnable end-to-end demo.
