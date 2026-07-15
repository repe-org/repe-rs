//! Stream a large in-memory value to a client **with end-to-end content
//! integrity computed in a single serialization pass** — the SVS write-side
//! producer ([`RouterValueStreamExt::with_writer_stream`]).
//!
//! `with_value_stream` (see `value_stream.rs`) hands the engine the value and the
//! app never sees the encode. `with_writer_stream` instead gives the app the
//! `Write` sink, so it can serialize the value straight into the stream while
//! teeing the bytes through a hasher and appending the digest as a trailer — the
//! value is proven intact end to end without ever encoding it twice.
//!
//! The stream is tagged [`BodyFormat::RawBinary`] (it is `payload || digest`, not
//! a bare BEVE value). The consumer pulls it with [`pull_to_file_trailer_verified`]:
//! one call holds the trailer back, hashes only the payload prefix, verifies the
//! digest, and commits the payload ALONE — no hand split, no re-truncate — so the
//! committed file is a bare, directly-decodable BEVE value. The digest covers the
//! logical, pre-compression bytes; `opts.compression` is applied by the engine
//! afterward and is transparent end to end.
//!
//! Run with: `cargo run --example writer_stream_digest --features value-stream`

use repe::value_stream::{RouterValueStreamExt, StreamOpts};
use repe::{BodyFormat, Client, RepeError, Router, Server, pull_to_file_trailer_verified};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::net::TcpListener;
use std::thread;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Dataset {
    name: String,
    samples: Vec<f64>,
}

// A tiny non-cryptographic digest (FNV-1a, 64-bit) so the example pulls in no
// hashing dependency. Swap in BLAKE3 / SHA-256 for real use — the seam is the
// same.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A `Write` that folds the bytes it receives into an FNV-1a hash and nothing else
/// — the consumer-side digest accumulator handed to `pull_to_file_trailer_verified`,
/// which feeds it the payload prefix (the trailer is held back) and then calls the
/// `verify` closure with the finalized hash.
struct FnvHasher(u64);

impl FnvHasher {
    fn new() -> Self {
        Self(FNV_OFFSET)
    }
    fn finish(self) -> u64 {
        self.0
    }
}

impl Write for FnvHasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A `Write` that hashes the bytes it forwards — the app-side tee the producer
/// closure serializes through, so the digest falls out of the one encode pass.
struct HashTee<'a> {
    inner: &'a mut dyn Write,
    hash: u64,
}

impl<'a> HashTee<'a> {
    fn new(inner: &'a mut dyn Write) -> Self {
        Self {
            inner,
            hash: FNV_OFFSET,
        }
    }
    fn finish(self) -> u64 {
        self.hash
    }
}

impl Write for HashTee<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        for &b in &buf[..n] {
            self.hash ^= b as u64;
            self.hash = self.hash.wrapping_mul(FNV_PRIME);
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dataset = Dataset {
        name: "demo".to_string(),
        samples: (0..1_000_000).map(|i| i as f64).collect(),
    };

    // Register a write-side producer. `resolve` hands back a closure that OWNS
    // the sink: it serializes the value straight into the stream, hashing as it
    // goes, then appends the 8-byte digest as a trailer — one pass, no buffering
    // of the whole encoded value and no second encode just to learn the digest.
    let server_copy = dataset.clone();
    let router = Router::new().with_writer_stream(
        BodyFormat::RawBinary,
        move |resource: &str| {
            (resource == "demo").then(|| {
                let value = server_copy.clone();
                move |w: &mut dyn Write| -> std::io::Result<()> {
                    let mut tee = HashTee::new(w);
                    beve::to_writer_streaming(&mut tee, &value)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    let digest = tee.finish();
                    w.write_all(&digest.to_le_bytes())?;
                    Ok(())
                }
            })
        },
        StreamOpts::default(),
    );

    let server = Server::new(router);
    let listener: TcpListener = server.listen("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        let _ = server.serve(listener);
    });

    // One call pulls the `payload || digest` stream, holds the 8-byte trailer back,
    // feeds only the payload to the FNV hasher, and calls `verify` with the
    // finalized hash and the claimed trailer. A match commits the payload ALONE
    // (trailer stripped) at `path`; a mismatch commits nothing and returns Err. No
    // hand split, no re-truncate — the committed file is a bare BEVE value.
    let client = Client::connect(addr)?;
    let path = std::env::temp_dir().join(format!("svs-writer-digest-{}.beve", std::process::id()));
    pull_to_file_trailer_verified(
        &client,
        "demo",
        &path,
        8,
        FnvHasher::new(),
        |hasher, trailer| {
            let claimed = u64::from_le_bytes(trailer.try_into().unwrap());
            if hasher.finish() == claimed {
                Ok(())
            } else {
                Err(RepeError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "content digest mismatch",
                )))
            }
        },
    )?;

    // Reaching here means the trailer verified and the payload-only file committed.
    let decoded: Dataset = beve::from_slice(&std::fs::read(&path)?)?;
    std::fs::remove_file(&path).ok();

    println!(
        "pulled '{}' with {} samples; integrity verified (trailer stripped, payload committed); \
         matches source: {}",
        decoded.name,
        decoded.samples.len(),
        decoded == dataset
    );
    Ok(())
}
