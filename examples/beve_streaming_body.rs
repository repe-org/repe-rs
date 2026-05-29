//! Stream BEVE-bodied REPE frames through a reusable scratch buffer.
//!
//! A frame-sending writer keeps a single `Vec<u8>` and serializes each message
//! body into it with `beve::to_vec_into`, which reuses the buffer's allocation
//! and clears it on every call. Paired with `write_message_streaming`, this
//! sends each frame with one BEVE encode, writes the header/query/body straight
//! to the sink (no separately-allocated wire frame), and -- once the buffer has
//! grown to fit the largest body -- performs no further allocation.
//!
//! Run with: `cargo run --example beve_streaming_body`

use repe::{BodyFormat, Header, QueryFormat, read_message, write_message_streaming};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Write};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct SensorFrame {
    id: u64,
    samples: Vec<f64>,
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

    // `sink` stands in for a socket; in a server this is your TcpStream.
    let mut sink: Vec<u8> = Vec::new();
    // One scratch buffer, reused for every frame's body.
    let mut body: Vec<u8> = Vec::new();

    for frame in &frames {
        // Single encode into the reused buffer (clears + keeps capacity).
        beve::to_vec_into(&mut body, frame)?;

        let mut header = Header::new();
        header.id = frame.id;
        header.query_format = QueryFormat::JsonPointer as u16;
        header.body_format = BodyFormat::Beve as u16;

        write_message_streaming(
            &mut sink,
            header,
            b"/ingest/frame",
            body.len() as u64,
            |w| w.write_all(&body),
        )?;

        println!(
            "sent frame {} ({} body bytes, scratch capacity {})",
            frame.id,
            body.len(),
            body.capacity()
        );
    }

    // Read the frames back off the wire and confirm they round-trip.
    let mut cursor = Cursor::new(&sink);
    for expected in &frames {
        let msg = read_message(&mut cursor)?;
        let decoded: SensorFrame = msg.beve_body()?;
        assert_eq!(&decoded, expected);
        println!(
            "read frame {} back ({} samples)",
            decoded.id,
            decoded.samples.len()
        );
    }

    println!("all {} frames round-tripped", frames.len());
    Ok(())
}
