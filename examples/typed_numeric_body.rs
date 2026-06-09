//! High-throughput numeric REPE bodies: typed numeric arrays and complex arrays
//! encoded and decoded in a single bulk copy, bypassing serde's per-element walk.
//!
//! When a whole message body is a contiguous numeric slice (`&[f64]`, `&[i32]`,
//! a `&[Complex<f64>]`), the typed-slice fast path frames and parses it in O(1)
//! work in the element count on little-endian targets, versus the element-by-
//! element traversal a serde body takes:
//!
//! * **Owned message** -- `MessageBuilder::body_typed_slice` /
//!   `body_complex_slice` encode the slice in one bulk write; the receiver pulls
//!   it back with `Message::decode_typed_slice` / `decode_complex_slice`.
//! * **Streaming, no body buffer** -- `write_message_typed_slice` sizes the body
//!   in closed form (`beve::typed_slice_size`) and writes the payload straight to
//!   the sink, so framing a multi-MiB `&[f64]` is a header write plus one bulk
//!   write.
//!
//! The bulk bytes are identical to what `body_beve(&Vec<T>)` produces, so the
//! fast path interoperates freely with a serde peer.
//!
//! Run with: `cargo run --example typed_numeric_body`

use repe::{
    Client, Complex, Header, Message, Router, Server, read_message, write_message_typed_slice,
};
use std::io::Cursor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- Owned message: a numeric Vec<f64> body ---------------------------
    let samples: Vec<f64> = (0..4096).map(|i| (i as f64 * 0.25).sin()).collect();

    let request = Message::builder()
        .query_str("/spectra/ingest")
        .body_typed_slice(&samples)
        .build();

    // The bulk encoder produced exactly the bytes a serde body would have.
    let serde_equivalent = Message::builder().body_beve(&samples)?.build();
    assert_eq!(request.body, serde_equivalent.body);

    let decoded: Vec<f64> = request.decode_typed_slice()?;
    assert_eq!(decoded, samples);
    println!(
        "typed Vec<f64>: {} elements, {} body bytes, round-tripped",
        decoded.len(),
        request.body.len()
    );

    // --- Owned message: a complex array body ------------------------------
    let spectrum: Vec<Complex<f64>> = (0..1024)
        .map(|i| Complex {
            re: (i as f64 * 0.1).cos(),
            im: (i as f64 * 0.1).sin(),
        })
        .collect();

    let complex_msg = Message::builder()
        .query_str("/spectra/complex")
        .body_complex_slice(&spectrum)
        .build();
    let back: Vec<Complex<f64>> = complex_msg.decode_complex_slice()?;
    assert_eq!(back, spectrum);
    println!(
        "complex Vec<Complex<f64>>: {} elements, {} body bytes, round-tripped",
        back.len(),
        complex_msg.body.len()
    );

    // --- Streaming a large numeric body with no intermediate body buffer ---
    // `write_message_typed_slice` sizes the body in O(1) and writes the payload
    // in one bulk write -- no `Message`, no separate body `Vec`.
    let big: Vec<f64> = (0..1_000_000).map(|i| i as f64).collect();
    let mut wire: Vec<u8> = Vec::new();
    write_message_typed_slice(&mut wire, Header::new(), b"/spectra/stream", &big)?;
    println!(
        "streamed {} f64 ({} wire bytes) via write_message_typed_slice",
        big.len(),
        wire.len()
    );

    // The streamed frame reads back like any other REPE message.
    let mut cursor = Cursor::new(&wire);
    let received = read_message(&mut cursor)?;
    let streamed_back: Vec<f64> = received.decode_typed_slice()?;
    assert_eq!(streamed_back.len(), big.len());
    assert_eq!(streamed_back, big);
    println!(
        "streamed body round-tripped: {} elements",
        streamed_back.len()
    );

    // --- High-level fast path: with_typed_slice + call_typed_slice --------------------
    // The same bulk path through the server/client API: a `with_typed_slice` route
    // decodes the request and frames the response as a typed numeric array (one
    // bulk copy each way), and `call_typed_slice` does the mirror on the client. No
    // per-element serde walk on either side, and the wire bytes are identical to
    // the serde path, so this interoperates with a `with_typed` peer.
    let router = Router::new()
        .with_typed_slice::<f64, f64, _>("/scale", |xs| Ok(xs.iter().map(|x| x * 2.0).collect()));
    let server = Server::new(router);
    let listener = server.listen("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    std::thread::spawn(move || {
        let _ = server.serve(listener);
    });

    let client = Client::connect(addr)?;
    let scaled: Vec<f64> = client.call_typed_slice("/scale", &samples)?;
    assert_eq!(scaled, samples.iter().map(|x| x * 2.0).collect::<Vec<_>>());
    println!(
        "call_typed_slice /scale: {} elements scaled over the wire via the bulk fast path",
        scaled.len()
    );

    Ok(())
}
