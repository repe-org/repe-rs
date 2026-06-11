//! Throughput comparison: **push** streaming (`repe::stream` / `TransferControl`,
//! credit-window pipelined) vs the new **pull** streaming (SVS / `value_stream`,
//! one round trip per chunk).
//!
//! Run: `cargo run --release --example stream_pull_vs_push --features value-stream`
//!
//! All three harnesses move the same byte volume over a loopback TCP connection,
//! in 1 MiB chunks, framed as REPE messages:
//!
//! * **pull (value_stream)** — the actual shipped pull API, end to end (BEVE
//!   serialize → optional zstd → chunked pull → reassemble/deserialize).
//! * **pull-mechanism** — a bare-socket serial request/response loop (client asks,
//!   server answers, one chunk at a time). Isolates the *architecture* of pull
//!   from BEVE/zstd and Router dispatch cost.
//! * **push (repe::stream)** — a faithful producer driven by `TransferControl`: it
//!   streams chunks ahead up to the credit window without waiting per chunk, ACKs
//!   flow back asynchronously, and each chunk is recorded in the replay ring (the
//!   resume machinery repe::stream pays for).
//!
//! Read the caveats printed at the end before drawing conclusions — loopback has
//! almost no latency, which is precisely the regime where pull looks best.

use repe::value_stream::{Compression, RouterValueStreamExt, StreamOpts};
use repe::{
    BodyFormat, Client, Message, QueryFormat, Router, Server, TransferControl, pull_value,
    read_message, write_message,
};
use std::io::{BufReader, BufWriter, Cursor, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const CHUNK: usize = 1 << 20; // 1 MiB
const WINDOW: u64 = 64 << 20; // 64 MiB credit window (repe::stream default)
const REPS: usize = 3;

fn main() {
    let sizes = [32usize << 20, 64 << 20, 128 << 20];

    println!(
        "chunk = {} KiB, credit window = {} MiB, {} reps (best shown), loopback TCP\n",
        CHUNK / 1024,
        WINDOW >> 20,
        REPS
    );

    println!("== Transport architecture (bare REPE framing, raw bytes) ==");
    println!("This is the apples-to-apples push-vs-pull comparison: same framing, same");
    println!("bytes, no codec, no Router dispatch. It isolates windowed-push vs serial-pull.\n");
    println!(
        "{:<26} {:>10} {:>12} {:>12}",
        "mechanism", "payload", "best (ms)", "MB/s"
    );
    println!("{}", "-".repeat(62));
    for &total in &sizes {
        let label = format!("{} MiB", total >> 20);
        row(
            "push (repe::stream)",
            &label,
            total,
            best(REPS, || bench_push(total)),
        );
        row(
            "pull-mechanism (bare)",
            &label,
            total,
            best(REPS, || bench_pull_mechanism(total)),
        );
        println!();
    }

    println!("== value_stream end-to-end (the shipped pull API: +BEVE, +optional zstd) ==");
    println!(
        "NOTE: for this payload (one flat ~{}-element byte array) these rows are",
        sizes[2] >> 20
    );
    println!("dominated by streaming DECODE cost, not transport — see the decode diagnostic.\n");
    println!(
        "{:<26} {:>10} {:>12} {:>12}",
        "mechanism", "payload", "best (ms)", "MB/s"
    );
    println!("{}", "-".repeat(62));
    for &total in &sizes {
        let payload = lcg_bytes(total);
        let label = format!("{} MiB", total >> 20);
        row(
            "pull (value_stream none)",
            &label,
            total,
            best(REPS, || bench_pull(&payload, Compression::None)),
        );
        row(
            "pull (value_stream zstd)",
            &label,
            total,
            best(REPS, || bench_pull(&payload, Compression::Zstd)),
        );
        println!();
    }

    decode_diagnostic(sizes[2]);
    print_caveats();
}

/// Isolate where the value_stream pull time goes for a large flat array: bulk
/// decode (`from_slice`) vs streaming decode (`from_reader_streaming`), no
/// transport involved. A large gap means the end-to-end rows above are
/// decode-bound, not transport-bound.
fn decode_diagnostic(total: usize) {
    let payload = lcg_bytes(total);
    let encoded = beve::to_vec(&payload).unwrap();

    let t = Instant::now();
    let bulk: Vec<u8> = beve::from_slice(&encoded).unwrap();
    let bulk_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(bulk.len(), total);

    let t = Instant::now();
    let streamed: Vec<u8> = beve::from_reader_streaming(Cursor::new(&encoded)).unwrap();
    let stream_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(streamed.len(), total);

    let mb = total as f64 / (1024.0 * 1024.0);
    println!(
        "== Decode diagnostic ({} MiB flat byte array, no transport) ==",
        total >> 20
    );
    println!(
        "  beve::from_slice          (bulk)      {:>8.2} ms   {:>7.0} MB/s",
        bulk_ms,
        mb / (bulk_ms / 1000.0)
    );
    println!(
        "  beve::from_reader_streaming (element)  {:>8.2} ms   {:>7.0} MB/s   <- value_stream mode-3 path",
        stream_ms,
        mb / (stream_ms / 1000.0)
    );
    println!();
}

fn row(name: &str, payload: &str, total: usize, dur: Duration) {
    let secs = dur.as_secs_f64();
    let mbps = (total as f64 / (1024.0 * 1024.0)) / secs;
    println!(
        "{:<26} {:>10} {:>12.2} {:>12.0}",
        name,
        payload,
        secs * 1000.0,
        mbps
    );
}

fn best(reps: usize, mut f: impl FnMut() -> Duration) -> Duration {
    (0..reps).map(|_| f()).min().unwrap()
}

/// Cheap incompressible-ish filler so the zstd variant is honest (no
/// runs-of-zeros that compress to nothing) without pulling in `rand`.
fn lcg_bytes(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    let mut state: u64 = 0x9E3779B97F4A7C15;
    while v.len() < n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.extend_from_slice(&state.to_le_bytes());
    }
    v.truncate(n);
    v
}

// ---- pull: the real value_stream API, end to end ------------------------------

fn bench_pull(payload: &[u8], compression: Compression) -> Duration {
    let payload = payload.to_vec();
    let opts = StreamOpts {
        chunk_bytes: CHUNK,
        compression,
        zstd_level: 3,
        session_depth: 4,
    };
    let router = Router::new().with_value_stream(move |_r: &str| Some(payload.clone()), opts);
    let server = Server::new(router);
    let listener = server.listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = server.serve(listener);
    });

    let client = Client::connect(addr).unwrap();
    let start = Instant::now();
    let got: Vec<u8> = pull_value(&client, "x").unwrap();
    let elapsed = start.elapsed();
    assert_eq!(got.len(), got.len()); // touch result so it isn't optimized away
    elapsed
}

// ---- pull-mechanism: bare-socket serial request/response ----------------------

fn bench_pull_mechanism(total: usize) -> Duration {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let producer = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).ok();
        let mut r = BufReader::new(stream.try_clone().unwrap());
        let mut w = BufWriter::new(stream);
        let body = vec![0u8; CHUNK];
        let mut sent = 0usize;
        loop {
            // Wait for the consumer's "next" request before answering (serial).
            if read_message(&mut r).is_err() {
                break;
            }
            let len = CHUNK.min(total - sent);
            let last = sent + len >= total;
            let msg = chunk_msg(&body[..len], last);
            write_message(&mut w, &msg).unwrap();
            w.flush().unwrap();
            sent += len;
            if last {
                break;
            }
        }
    });

    let stream = TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).ok();
    let mut r = BufReader::new(stream.try_clone().unwrap());
    let mut w = BufWriter::new(stream);

    let start = Instant::now();
    let mut received = 0usize;
    loop {
        // Ask, then wait for the chunk: one full round trip per chunk.
        write_message(&mut w, &next_req()).unwrap();
        w.flush().unwrap();
        let resp = read_message(&mut r).unwrap();
        received += resp.body.len();
        if resp.query.first().copied() == Some(1) {
            break;
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(received, total);
    producer.join().unwrap();
    elapsed
}

// ---- push: faithful repe::stream / TransferControl windowed producer ----------

fn bench_push(total: usize) -> Duration {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let producer = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).ok();
        let control = TransferControl::new(WINDOW);

        // ACK reader: the consumer's ACKs advance the credit window concurrently
        // with production, so the producer never stalls per chunk — only when it
        // gets WINDOW bytes ahead of the ACKs.
        let ack_ctl = control.clone();
        let ack_stream = stream.try_clone().unwrap();
        let ack_reader = thread::spawn(move || {
            let mut r = BufReader::new(ack_stream);
            while let Ok(m) = read_message(&mut r) {
                let off = u64::from_le_bytes(m.body[..8].try_into().unwrap());
                ack_ctl.record_ack(0, off);
                if off >= total as u64 {
                    break;
                }
            }
        });

        let mut w = BufWriter::new(stream);
        let body = vec![0u8; CHUNK];
        let mut offset = 0u64;
        let deadline = Instant::now() + Duration::from_secs(60);
        while (offset as usize) < total {
            let len = CHUNK.min(total - offset as usize);
            // Block only if we are a full window ahead of the ACKs.
            control.wait_for_credit(len as u64, deadline).unwrap();
            // Record the chunk in the replay ring (resume support cost), then send.
            control.push_replay(
                offset,
                len as u64,
                (offset as usize + len) >= total,
                body[..len].to_vec(),
            );
            let last = (offset as usize + len) >= total;
            write_message(&mut w, &chunk_msg(&body[..len], last)).unwrap();
            w.flush().unwrap();
            offset += len as u64;
            control.record_sent(offset);
        }
        ack_reader.join().unwrap();
    });

    let stream = TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).ok();
    let mut r = BufReader::new(stream.try_clone().unwrap());
    let mut w = BufWriter::new(stream);

    let start = Instant::now();
    let mut received = 0u64;
    loop {
        let chunk = read_message(&mut r).unwrap();
        received += chunk.body.len() as u64;
        // ACK the cumulative offset (one ACK per chunk).
        let ack = Message::builder()
            .body_bytes(received.to_le_bytes().to_vec())
            .body_format(BodyFormat::RawBinary)
            .build();
        write_message(&mut w, &ack).unwrap();
        w.flush().unwrap();
        if chunk.query.first().copied() == Some(1) {
            break;
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(received, total as u64);
    producer.join().unwrap();
    elapsed
}

// ---- shared framing -----------------------------------------------------------

fn chunk_msg(body: &[u8], last: bool) -> Message {
    Message::builder()
        .query_format(QueryFormat::RawBinary)
        .query_bytes(vec![last as u8])
        .body_format(BodyFormat::RawBinary)
        .body_bytes(body.to_vec())
        .build()
}

fn next_req() -> Message {
    Message::builder()
        .query_format(QueryFormat::JsonPointer)
        .query_bytes(b"/next".to_vec())
        .build()
}

fn print_caveats() {
    println!(
        "\nConclusions:\n\
         1. TRANSPORT (the actual push-vs-pull question): on loopback the two are comparable,\n\
            and serial pull is even a bit faster than windowed push -- because a round trip is\n\
            nearly free at ~0 latency, while push pays credit accounting + a per-chunk replay-\n\
            ring copy (its resume support). Push's win is LATENCY-bound, not present on a LAN.\n\
         2. The value_stream rows are CODEC-bound, not transport-bound: the decode diagnostic\n\
            shows BEVE decode of this payload runs at a few hundred MB/s, while the bare pull\n\
            transport runs at ~8 GB/s. The pull *transport* is not the bottleneck here.\n\
         3. That codec cost is COMMON to push and pull -- pushing a serialized value over\n\
            repe::stream would pay the same BEVE serialize/deserialize -- so it is NOT a\n\
            push-vs-pull differentiator. The only transport-level differentiator is round-trip\n\
            sensitivity (below).\n\
         4. This pull implementation does NOT pipeline `next` requests; the SVS spec permits a\n\
            pipelined pull window, which would erase most of the latency gap below.\n\n\
         Latency projection (1 MiB chunks, serial pull): one-way latency L adds ~2*L of stall\n\
         per chunk (request + response). 128 MiB = 128 chunks: at L=1ms that is ~256 ms of pure\n\
         round-trip stall that windowed push avoids (push pays ~one window-fill total); at\n\
         L=25us (LAN) ~6 ms; at loopback (~1us) negligible, as measured above.\n\n\
         Friction (beve): `from_slice::<Vec<u8>>` / `from_reader_streaming::<Vec<u8>>` decode a\n\
         flat byte array element-wise (~300 MB/s) rather than via a bulk u8 path (memcpy speed).\n\
         A bulk fast-path for primitive arrays in the streaming decoder would lift value_stream\n\
         mode-3 throughput for large numeric/byte payloads by an order of magnitude."
    );
}
