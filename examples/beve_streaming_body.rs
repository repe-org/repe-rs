//! Stream BEVE-bodied REPE frames without building a wire-frame `Vec`.
//!
//! Two patterns feed a serialized body to `write_message_streaming`, which
//! writes the header/query/body straight to the sink (no separately-allocated
//! wire frame):
//!
//! * **Reusable scratch buffer** -- serialize each body once into a single
//!   `Vec<u8>` with `beve::to_vec_into`, which reuses the allocation and clears
//!   it on every call. One encode per frame and, once the buffer has grown to
//!   fit the largest body, no further allocation. The default for a writer
//!   sending many frames whose bodies fit in memory.
//! * **Zero-buffer streaming** -- measure the encoded length with
//!   `beve::serialized_size`, then stream the body to the sink with
//!   `beve::to_writer_streaming`. The body is never materialized. This costs a
//!   size pass over the value (O(1) for a `&[u8]`/`serde_bytes` blob or a
//!   `beve::TypedSlice<T>`, O(payload) for a nested structure); reach for it
//!   when the body is too large to hold in memory at once.
//!
//! Run with: `cargo run --example beve_streaming_body`

use repe::{BodyFormat, Header, QueryFormat, write_message_streaming};
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct SensorFrame {
    id: u64,
    samples: Vec<f64>,
}

fn header_for(frame: &SensorFrame) -> Header {
    let mut header = Header::new();
    header.id = frame.id;
    header.query_format = QueryFormat::JsonPointer as u16;
    header.body_format = BodyFormat::Beve as u16;
    header
}

/// Pattern 1: one scratch `Vec` reused across every frame's body.
fn send_with_scratch(frames: &[SensorFrame]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut sink: Vec<u8> = Vec::new();
    // One scratch buffer, reused for every frame's body.
    let mut body: Vec<u8> = Vec::new();

    for frame in frames {
        // Single encode into the reused buffer (clears + keeps capacity).
        beve::to_vec_into(&mut body, frame)?;
        write_message_streaming(
            &mut sink,
            header_for(frame),
            b"/ingest/frame",
            body.len() as u64,
            |w| w.write_all(&body),
        )?;
        println!(
            "  scratch: sent frame {} ({} body bytes, scratch capacity {})",
            frame.id,
            body.len(),
            body.capacity()
        );
    }
    Ok(sink)
}

/// Pattern 2: measure with `serialized_size`, then stream straight to the sink
/// -- the encoded body is never held in memory.
fn send_zero_buffer(frames: &[SensorFrame]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut sink: Vec<u8> = Vec::new();

    for frame in frames {
        let body_len = beve::serialized_size(frame)?;
        // body_writer returns beve::Result directly: its error type only needs
        // to be Into<RepeError>, so a beve encode error stays a RepeError::Beve.
        write_message_streaming(
            &mut sink,
            header_for(frame),
            b"/ingest/frame",
            body_len,
            |w| beve::to_writer_streaming(w, frame),
        )?;
        println!(
            "  zero-buffer: sent frame {} ({body_len} body bytes)",
            frame.id
        );
    }
    Ok(sink)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let frames = [
        SensorFrame {
            id: 1,
            samples: vec![0.5; 8],
        },
        SensorFrame {
            id: 2,
            samples: vec![1.25; 4096],
        },
        SensorFrame {
            id: 3,
            samples: vec![-3.0; 64],
        },
    ];

    println!("reusable scratch buffer:");
    let scratch_wire = send_with_scratch(&frames)?;
    println!("zero-buffer streaming:");
    let zero_buffer_wire = send_zero_buffer(&frames)?;

    // Both patterns frame the same logical messages and round-trip identically.
    // Reading frames back-to-back from one cursor also proves serialized_size
    // framed each body exactly: an over- or under-count would desync the next
    // frame's header and the read would fail.
    for (label, wire) in [
        ("scratch", &scratch_wire),
        ("zero-buffer", &zero_buffer_wire),
    ] {
        let mut cursor = std::io::Cursor::new(wire);
        for expected in &frames {
            let msg = repe::read_message(&mut cursor)?;
            let decoded: SensorFrame = msg.beve_body()?;
            assert_eq!(&decoded, expected);
        }
        println!("{label}: all {} frames round-tripped", frames.len());
    }

    Ok(())
}
