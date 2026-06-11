# Streaming Serialized Values (proposal)

Status: **implemented** behind the `value-stream` feature (`src/value_stream.rs`). SVS is a **pull-only** contract, so this is the whole of it on the wire; an **async / WebSocket client puller** is the one remaining piece (see below). Companion to [Streaming with Backpressure](streaming.md); this is the bulk-transfer shape for *serialized, compressed, unknown-length* payloads.

## 0. What is implemented

Behind the optional `value-stream` feature (pulls in `zstd`):

- **Server producer** — `Router::with_value_stream(resolve, StreamOpts)` registers the `/_svs/{open,next,cancel}` routes backed by a bounded per-stream session (a `std::thread` running `beve::to_writer_streaming` → optional zstd `write::Encoder` → a `sync_channel` of chunks). One registration serves one value type. The `next` handler reports `Execution::OffReader` so it is also correct on the WebSocket server; it blocks for backpressure on the synchronous `Server` (one thread per connection). It is **not** suitable on the inline `AsyncServer`, which does not honor `OffReader` (documented in the module).
- **Client puller** — `pull_value::<T>`, `pull_to_beve_file`, `pull_to_beve_zst_file`, and the general `pull_stream` drive the pull over the synchronous `Client`. All three receiver modes (§3.2) work: raw `.beve.zst`, decompressed `.beve`, and streaming typed decode into `T` via `beve::from_reader_streaming`. File modes write a temp sibling and rename only after the terminating `last = 1` and an fsync, so a dropped connection never commits a truncated file.
- **Tests** — unit tests for the chunker and the lookahead/`last` detection (including producer-failure vs clean-end), plus end-to-end TCP round-trips for every mode, tag/output-mismatch rejection, unknown-resource errors, sequential-stream release, and the atomic-file guarantee (`tests/value_stream.rs`). A runnable demo is in `examples/value_stream.rs`.

**Remaining:** an async/WebSocket-client puller (the server producer already runs under WebSocket; only the client side is sync-TCP-only today).

**Out of scope (not deferred):** client→server *push* upload. SVS is pull-only by design (spec §7). A client that needs to send a value to a server either has the server pull from it (the same `open`/`next`/`cancel`, roles reversed, on a bidirectional transport like WebSocket) or uses the separate credit-windowed push engine `repe::stream` ([Streaming with Backpressure](streaming.md)) — which already provides ACK-window backpressure and reconnect-resume. SVS does not reinvent a weaker one-outstanding-ACK push; see §5 and §11.


Revision note: an earlier draft used a *server-push* design backpressured by "blocking writes." That is unsound — `WebSocketClient::subscribe_notifies` pushes every inbound notify into an **unbounded** channel (its own docs warn a slow consumer "will grow the buffer until the process OOMs" and name an ACK window as the fix), so transport flow control never reaches the producer. This revision uses a **client-pull (request/response)** model, where flow control is intrinsic and no inbound-notify subscription is involved. See §5–§6.

## 1. The problem

Move a large in-memory value to a peer over a TCP or WebSocket REPE connection, where the wire payload is **BEVE-serialized and zstd-compressed** and is **multi-GB even compressed**, such that:

- the **producer** never materializes a full serialized or compressed copy (only the value it already holds, plus codec buffers);
- the **receiver** chooses, per transfer, one of:
  1. write a `.beve.zst` file (compressed, as received),
  2. write a `.beve` file (decompressed),
  3. deserialize into a `T: DeserializeOwned` in memory (streaming decode);
- for outputs 1 and 2 the value is **never** deserialized; for output 3 only the resulting `T` is resident, never a full compressed or decompressed buffer.

Neither existing transfer mechanism fits directly (§5).

## 2. The pieces already exist

This proposal is mostly glue, because the hard parts are already public:

- **`beve` streams both directions, including typed decode.** `beve::to_writer_streaming::<W: Write, T: Serialize>` + `beve::serialized_size` produce without materializing; `beve::from_reader_streaming::<R: Read, T: DeserializeOwned>` decodes **directly into `T`** token-by-token, with no intermediate generic tree (`streaming_de.rs`). This is a genuine advantage over the Julia binding, whose typed deserialize still materializes a generic tree first — Rust needs no `beve` change for any mode.
- **zstd streams.** The `zstd` crate's `stream::write::Encoder<W>` / `stream::write::Decoder<W>` (write-side filters) and `stream::read::Decoder<R>` (read-side filter) wrap any `Write`/`Read`. `zstd` is **not yet a dependency** — add it behind an optional feature (§7).
- **Request/response works on both transports today.** The pull model below uses ordinary request/response, which the sync `Client`, async client, and `WebSocketClient` all already provide. It deliberately does **not** use `subscribe_notifies` (see the revision note).

So the only genuinely new code is a chunk-transport adapter (a producer session + a pull reader) and the receiver wiring.

## 3. Architecture (client-pull)

```
SERVER (producer, holds value: T)              CLIENT (receiver, drives the pull)
  &value                                         loop: `next(stream_id)` request
    │ beve::to_writer_streaming                    │  ← response body = next chunk (+ last flag)
    ▼                                              ▼
  BEVE bytes                                     chunk bytes
    │ zstd write::Encoder                          ├─► write raw ──────────────► tmp → rename .beve.zst (mode 1)
    ▼                                              ├─► zstd write::Decoder ────► tmp → rename .beve     (mode 2)
  zstd bytes                                       └─► zstd read::Decoder
    │ BoundedSink (blocks task when full)               │ beve::from_reader_streaming::<_, T>   (mode 3)
    ▼                                                    ▼
  bounded session channel ──[ `next` request/response over TCP / WebSocket ]── pulled on demand
```

The server holds a per-stream **session**: a `spawn_blocking` task running serialize→compress into a **bounded** channel. The client pulls chunks with ordinary `next` requests at its own pace. Request/response is the flow control (§6): the bounded channel stalls the serializer when the client is slow; the client only asks for more when ready. No notify subscription, no unbounded queue, no ACK window.

### 3.1 Producer (server session)

```rust
// On `open`: start a streaming session for `value`, return a stream_id.
let (tx, rx) = sync_channel::<Vec<u8>>(SESSION_DEPTH);   // bounded — backpressure lives here
std::thread::spawn(move || {                             // or spawn_blocking in async
    let sink = ChunkSink::new(tx, 1 << 20);              // ~1 MiB chunks into the bounded channel
    let mut enc = zstd::stream::write::Encoder::new(sink, 3)?;
    beve::to_writer_streaming(&mut enc, &value)?;        // value -> BEVE -> zstd -> bounded chunks
    let sink = enc.finish()?;                            // flush zstd epilogue into sink
    sink.finish()                                        // push final partial chunk, then drop tx (closes rx)
});
// On `next(stream_id)`: rx.recv() -> response body; channel closed & drained -> last = true.
// On last / `cancel` / client disconnect: ensure the thread is joined/stopped and the session is freed.
```

Peak server memory = the value (already resident) + `SESSION_DEPTH` chunks + zstd buffers. The serializer **blocks** on a full `sync_channel`, so a slow client cannot make it race ahead. Session lifetime must be bounded: free it on `last`, on `cancel`, and on client disconnect/idle-timeout, or an abandoned pull leaks the thread and the value. Teardown must **drop the receiver / close the channel** to release a serializer thread already parked inside `SyncSender::send` — dropping `rx` wakes that parked send (with an error it treats as cancellation); merely forgetting the session would leave the thread blocked forever.

There is no upload (client-producer) counterpart: SVS is pull-only. When the producer is the client rather than the server, the *server* pulls from it with the same `open`/`next`/`cancel` on a bidirectional transport, or a genuine push is carried by `repe::stream` instead — see the out-of-scope note at the top and §11.

### 3.2 Receiver (three modes; client drives the pull)

```rust
// Mode 1: .beve.zst — write compressed bytes to a TEMP file, rename on `last`.
tmp.write_all(&chunk)?;                      // ... on last: tmp.sync_all()?; fs::rename(tmp, final)?;

// Mode 2: .beve — stream-decompress to a TEMP file, rename on `last`.
let mut dec = zstd::stream::write::Decoder::new(tmp)?;
dec.write_all(&chunk)?;                      // ... on last: dec.flush()?; rename(tmp, final)?;

// Mode 3: struct — pull reader -> zstd read::Decoder -> typed streaming decode.
//   `ChunkReader: Read` issues `next` requests on demand (§3.3), bounded in-flight.
let value: T = beve::from_reader_streaming(
    zstd::stream::read::Decoder::new(ChunkReader::new(client, stream_id))?
)?;
```

| Mode | Output | Holds value? | Holds full compressed/decompressed buffer? |
|---|---|---|---|
| 1 | `.beve.zst` | no | no |
| 2 | `.beve` | no | no |
| 3 | `T` | yes (the result) | no — typed decode streams directly into `T` |

Mode 3 is strictly leaner than `beve::from_reader` over a whole decompressed buffer, and far leaner than read-file-then-decompress-then-decode (which holds compressed + decompressed + value at once). Because `from_reader_streaming` decodes straight into `T`, there is no intermediate generic tree (unlike the Julia path).

**Atomic output (mandatory).** The wire commits no total length (the compressed size is unknown up front), so a dropped connection would otherwise leave a byte-truncated `.beve`/`.beve.zst` that looks complete. File modes write to a temp path and `rename` only **after** `last` and a flush. A stream that ends without `last` is discarded.

### 3.3 Pull reader for mode 3

`from_reader_streaming` pulls bytes (`R: Read`), which fits the pull model directly: `ChunkReader::read` returns buffered bytes and, when empty, issues the next `next` request (and surfaces `last` as EOF). For throughput, the client may keep a small in-flight window of `next` requests and feed a **bounded** channel that the blocking `from_reader_streaming` (run under `spawn_blocking` in async code) drains — bounded, unlike the push design's unbounded notify channel. Modes 1 and 2 need no decoder; the pull loop writes bytes straight to the temp file/decoder.

## 4. Wire contract — defined in the shared SVS spec

Because a REPE.jl service and a repe-rs service will exchange these streams, the wire is defined **once** in the shared, cross-language contract — [REPE Serialized Value Stream (SVS)](https://github.com/repe-org/REPE/blob/main/serialized-stream-protocol.md), canonical in the REPE org repo — not in this plan. That document is the normative source for the routes (`/_svs/{open,next,cancel}`), the message shapes, and the exact byte encodings (the `stream_id` placement, the `last`-flag in the `next` response query, and the `format`/`compression` fields — `format` reusing REPE `BodyFormat` values, `compression` orthogonal). This plan implements that contract.

In brief: `open` (client → server, returns `stream_id`, `format`, `compression`), `next` (client → server with `{stream_id}`, response carries `last` in the query and the raw chunk in the body, zero-copy), `cancel`. No total length is committed; `last = 1` terminates. **repe-rs is the reference implementation** that validates the contract's provisional v0 before it freezes as v1 (see that doc's status).

## 5. Why the existing mechanisms don't fit

- **`write_message_streaming` (single-message).** Requires `body_len` in the header before the first body byte. The zstd-compressed length is unknown until compression finishes (a size pass would mean compressing twice), so the single-message path cannot carry it. Over WebSocket it also reassembles the whole frame in memory, defeating the bound. Ruled out.
- **`repe::stream` (the backpressure engine).** Works, but carries a credit window, replay ring, and resume handshake this case does not need: pull provides backpressure intrinsically (§6) and v1 does not need resume. Using it here is over-machinery for the pull case. It is, however, the right home for genuine client→server *push* and for resume — those are `repe::stream`'s job, not a future SVS direction (§11).
- **Ranged pull.** For *seekable* sources accessed by offset. Serialized + compressed output is forward-only and non-seekable, so random-access ranges do not apply.

This proposal is the lightweight middle: client-pull request/response, intrinsically backpressured, no resume, for unknown-length serialized payloads.

## 6. Backpressure and direction (the corrected core)

- **Why pull, not push.** A notify push gives no free backpressure: `subscribe_notifies` drains the socket into an unbounded channel (its docs warn of OOM and name an ACK window as the fix), so a fast producer + slow disk grows the buffer without limit and TCP/WS flow control never reaches the producer. The earlier "blocking writes" claim was wrong for every WebSocket path. **Pull** fixes this structurally: the client requests the next chunk only when ready, and the server's bounded session channel (§3.1) stalls the serializer when the client lags. The round trip *is* the flow control — no ACK window, no credit accounting, and no new transport primitive.
- **Throughput vs. round trips.** Pull costs one round trip per chunk (~8k for 8 GB at 1 MiB chunks): negligible on a LAN, significant at WAN latency. Mitigate by **pipelining** a small window of `next` requests via existing multiplexing (tag responses with a sequence, reassemble in order); this hides RTT while staying bounded by the window. Oversizing chunks is not the answer on WebSocket (§9).
- **One direction, both role assignments.** The wire is always consumer-pulls-producer over request/response. Usually the server is the producer and the client pulls (above). When the client is the producer, the server pulls from it the same way on a bidirectional transport — there is no separate client-push path (that is `repe::stream`'s job, not SVS's; see §5 and §11).

## 7. Dependency and feature gating

Add `zstd` behind an **optional feature** so the core crate stays lean:

```toml
[features]
beve-zstd-stream = ["dep:zstd"]   # streaming zstd for serialized-value transfer

[dependencies]
zstd = { version = "0.13", optional = true }
```

`beve` is already a dependency and already exposes the full streaming API (including typed decode); no `beve` change is required. The transfer helpers live behind `beve-zstd-stream`.

## 8. Proposed surface

```rust
// Server: register open/next handlers backed by a bounded session channel.
pub fn serve_value_stream<F, T>(server: &mut Server, resolve: F)
where F: Fn(&str) -> Option<T> + Send + Sync + 'static, T: Serialize + Send + 'static;

pub struct StreamOpts { pub chunk_bytes: usize, pub zstd_level: i32, pub session_depth: usize }

// Client: pull a stream into the chosen output. File modes write-temp-then-rename.
pub enum StreamOutput<'a, T> {
    BeveZstdFile(&'a Path),   // mode 1
    BeveFile(&'a Path),       // mode 2
    Value(PhantomData<T>),    // mode 3 -> returns T
}

pub fn pull_stream<T: DeserializeOwned>(client: &Client, resource: &str, out: StreamOutput<T>)
    -> Result<Option<T>, RepeError>;   // Some(T) for mode 3, None for file modes
```

(Names illustrative; align with crate conventions. Async variants mirror these, with `spawn_blocking` around the blocking codec passes per §3.3.)

## 9. Chunk sizing

Keep `chunk_bytes` small enough that a WebSocket binary frame stays modest — **1–4 MiB**. Smaller favors WebSocket receiver memory and latency; larger favors TCP throughput by cutting per-message overhead. Default 1 MiB serves both transports; expose it. Do **not** use tens-of-MiB chunks — that reintroduces large WS frames and undoes the receiver bound. For WAN throughput prefer pipelined pull (§6) over oversizing.

## 10. Memory accounting (multi-GB payload)

| | Server (producer) | Client mode 1/2 (file) | Client mode 3 (struct) |
|---|---|---|---|
| Value / result | resident (source) | — | resident (result `T`) |
| Full serialized buffer | never | never | never |
| Full compressed buffer | never | never | never |
| Full decompressed buffer | never | never | never (typed decode streams) |
| Transient | `SESSION_DEPTH` chunks + zstd | 1 chunk + zstd + write buf | window chunks + zstd + decode |

File modes are bounded regardless of payload size; struct mode is bounded to `T` + O(window). (Contrast the Julia companion plan, where typed mode 3 is bounded only after a new streaming typed deserializer lands; Rust's `from_reader_streaming` already streams typed decode.)

## 11. Out of scope and deferred

**Out of scope of SVS entirely** (these are `repe::stream`'s domain, not a future SVS direction — SVS is the pull contract, `repe::stream` is the push engine):

- **Client→server push / pipelined-push throughput.** If a producer-driven transfer with a credit/ACK window is ever needed (e.g. pull's RTT ceiling proves insufficient over WAN even with pipelining, §6), that is `repe::stream`, which already provides the ACK-window backpressure. It is a distinct protocol, not a knob added to SVS.
- **Resume across a dropped connection.** SVS restarts a failed transfer (discarding the un-renamed temp file). Resuming a stateful serialize+compress stream means re-running it to the resume offset, and the replay machinery for it lives in `repe::stream`. SVS commits no resume state.

**Deferred (could land under SVS later):**
- **Integrity check.** Transport already checksums; atomic rename (§3.2) already prevents a *truncated* file being accepted. For corruption detection, fold a streaming CRC32C into the final chunk/`end` (codec passes are single-pass, so cheap).
- **Multiple concurrent streams per connection.** `stream_id` namespaces them; routing N in flight is additive.

## 12. Build order

1. Add `zstd` optional dep + `beve-zstd-stream` feature.
2. `ChunkSink: Write` + bounded session channel + `open`/`next`/`cancel` helpers over `Message`; test the producer session in isolation (bounded memory, clean shutdown), asserting byte-identity with `to_writer_streaming` + zstd over a `Vec`.
3. `ChunkReader: Read` + `pull_stream` with file modes (atomic temp-then-rename) + round-trip over TCP.
4. Mode 3: typed `from_reader_streaming` over the pull reader (`spawn_blocking` in async); round-trip a struct, asserting value-equality with the source.
5. WebSocket parity — free, since pull is plain request/response; assert frame sizes stay ~`chunk_bytes`.
6. Backpressure (slow client → bounded server memory), session-lifecycle (disconnect frees the session), atomic-output (mid-stream kill leaves no final-named file), cancel, and large-payload (generated) tests.
