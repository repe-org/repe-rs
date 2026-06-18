//! Streaming transfer of a serialized, optionally zstd-compressed in-memory
//! value over ordinary REPE request/response — the **Serialized Value Stream
//! (SVS)** wire contract.
//!
//! SVS moves a large value (multi-GB even compressed) from a producer to a
//! consumer without either side materializing a full serialized or compressed
//! copy. The value is BEVE-serialized and, on the compressed path, zstd-encoded
//! as it is produced; the bytes cross the wire as a sequence of opaque chunks
//! pulled one round trip at a time, so flow control is the request/response
//! cadence itself — no ACK window, no unbounded queue. The receiver chooses, per
//! transfer, to write the compressed bytes to a file, decompress them to a file,
//! or stream-decode them straight into a `T`.
//!
//! A producer may also stream **opaque, already-serialized bytes** verbatim from
//! any [`Read`] ([`RouterValueStreamExt::with_reader_stream`]) rather than
//! re-serializing a value. That is the fit when the bytes already exist in their
//! final on-disk form: the consumer pulls them with [`pull_to_file`] /
//! [`pull_to_file_async`] (or the format-agnostic [`pull_consume_async`]) and,
//! with [`Compression::None`], writes a byte-identical copy. SVS does not commit
//! a content digest on the wire, so when end-to-end integrity matters the
//! producer and consumer agree on one out of band and the consumer recomputes it
//! over the streamed bytes with [`pull_to_file_verified_async`], which vetoes the
//! atomic rename on a mismatch.
//!
//! This module is the repe-rs **reference implementation** of the SVS contract.
//! The normative wire definition (routes, `stream_id` encoding, the `last`-flag
//! placement, the `format`/`compression` tags) lives in the REPE org repository
//! next to the base REPE spec; everything in this module beyond those wire fields
//! (chunk sizing, the bounded producer channel, the lookahead that detects the
//! final chunk) is local engine policy and not part of the contract.
//!
//! # Directions
//!
//! * **Download** (server produces, client pulls) — the primary direction.
//!   Register a producer with [`RouterValueStreamExt::with_value_stream`] (a
//!   serde value), [`with_typed_value_stream`] / [`with_complex_value_stream`] (a
//!   bulk numeric / complex array), or [`with_reader_stream`] (opaque bytes from
//!   a [`Read`]). Pull with [`pull_value`] / [`pull_to_beve_file`] /
//!   [`pull_to_beve_zst_file`] / [`pull_to_file`] (or the general
//!   [`pull_stream`]) over the synchronous [`Client`]. From async code, drive an
//!   [`AsyncClient`] (or a `WebSocketClient`) with [`pull_value_async`] /
//!   [`pull_typed_slice_async`] / [`pull_complex_slice_async`] /
//!   [`pull_to_file_async`] / [`pull_to_file_verified_async`], or the
//!   format-agnostic [`pull_consume_async`]: these run the blocking decoder on a
//!   `spawn_blocking` thread fed by an async pull loop, so they never park the
//!   runtime.
//!
//! [`with_typed_value_stream`]: RouterValueStreamExt::with_typed_value_stream
//! [`with_complex_value_stream`]: RouterValueStreamExt::with_complex_value_stream
//! [`with_reader_stream`]: RouterValueStreamExt::with_reader_stream
//!
//! # Where the producer may run
//!
//! A `next` request blocks the handler until the producer has the next chunk
//! ready — that block *is* the backpressure. It must therefore run somewhere a
//! blocking handler is fine: the synchronous [`Server`](crate::Server) (one
//! thread per connection) or the WebSocket server (the `next` handler reports
//! [`Execution::OffReader`](crate::Execution), so it runs off the reader task).
//! The inline async TCP server ([`AsyncServer`](crate::AsyncServer)) runs handlers
//! on the connection's runtime task and does not honor `OffReader`, so a producer
//! registered there would park a runtime worker; use the sync or WebSocket server
//! for SVS producers.

use crate::async_client::AsyncClient;
use crate::client::Client;
use crate::constants::{BodyFormat, ErrorCode, QueryFormat};
use crate::error::RepeError;
use crate::message::{Message, create_error_message};
use crate::server::{Execution, HandlerErased, Router};
use beve::Complex;
use beve::from_slice as beve_from_slice;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

/// The SVS contract version this implementation speaks, reported in the `open`
/// response and validated by the client.
pub const SVS_VERSION: u8 = 1;

/// Reserved namespace for SVS routes. Routes beginning with `_` are reserved for
/// REPE protocol extensions; SVS claims `/_svs`.
pub const ROUTE_OPEN: &str = "/_svs/open";
/// Route for pulling the next chunk of a download stream. See [`ROUTE_OPEN`].
pub const ROUTE_NEXT: &str = "/_svs/next";
/// Route for releasing a stream early (request or notify). See [`ROUTE_OPEN`].
pub const ROUTE_CANCEL: &str = "/_svs/cancel";

/// The codec applied over the serialized value bytes, orthogonal to the
/// serialization `format`. Wire values match the SVS `compression` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Compression {
    /// `compression = 0`: the chunk stream is the serialized value verbatim.
    None = 0,
    /// `compression = 1`: the chunk stream is a zstd frame over the serialized
    /// value.
    Zstd = 1,
}

impl Compression {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Compression::None),
            1 => Some(Compression::Zstd),
            _ => None,
        }
    }
}

/// Local producer-side knobs (SVS §5 "local policy" — none of these are on the
/// wire, so a producer and consumer interoperate regardless of how they are set).
#[derive(Debug, Clone, Copy)]
pub struct StreamOpts {
    /// Target size of each chunk pushed into the session channel. Keep this a few
    /// MiB at most so a WebSocket binary frame stays modest; 1 MiB by default.
    pub chunk_bytes: usize,
    /// Codec applied over the serialized bytes.
    pub compression: Compression,
    /// zstd compression level when `compression` is [`Compression::Zstd`].
    pub zstd_level: i32,
    /// Bounded depth of the per-stream producer channel. This is where memory is
    /// bounded and backpressure lives: the serializer parks once this many chunks
    /// are buffered ahead of the consumer.
    pub session_depth: usize,
}

impl Default for StreamOpts {
    fn default() -> Self {
        Self {
            chunk_bytes: 1 << 20,
            compression: Compression::Zstd,
            zstd_level: 3,
            session_depth: 4,
        }
    }
}

// ---- wire message bodies (BEVE) ------------------------------------------------

#[derive(Serialize, Deserialize)]
struct OpenRequest {
    resource: String,
}

#[derive(Serialize, Deserialize)]
struct OpenResponse {
    version: u8,
    stream_id: u64,
    /// Serialization format of the logical content, using REPE `BodyFormat`
    /// values (`1` = BEVE here).
    format: u16,
    /// Codec over the serialized bytes (`0` = none, `1` = zstd).
    compression: u8,
}

#[derive(Serialize, Deserialize)]
struct NextRequest {
    stream_id: u64,
}

#[derive(Serialize, Deserialize)]
struct CancelRequest {
    stream_id: u64,
    reason: String,
}

// ---- server: producer session -------------------------------------------------

/// One message in the producer→handler channel. Terminating the stream with an
/// explicit [`Msg::End`] / [`Msg::Fail`] (rather than relying on the channel
/// closing) keeps the "did it finish cleanly?" answer ordered with the chunks: a
/// truncated production surfaces as `Fail`, never as a silently-short clean end.
enum Msg {
    Chunk(Vec<u8>),
    End,
    Fail(String),
}

/// A `Write` sink that batches the producer's byte stream into fixed-size chunks
/// and pushes each into the bounded session channel. A send failure (the receiver
/// was dropped — the consumer cancelled or disconnected) surfaces as a broken
/// pipe, which aborts the in-progress serialize/compress pass.
struct ChunkSink {
    tx: SyncSender<Msg>,
    buf: Vec<u8>,
    chunk_bytes: usize,
}

impl ChunkSink {
    fn new(tx: SyncSender<Msg>, chunk_bytes: usize) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(chunk_bytes),
            chunk_bytes,
        }
    }

    fn send_chunk(&mut self) -> io::Result<()> {
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(self.chunk_bytes));
        self.tx
            .send(Msg::Chunk(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "svs stream cancelled"))
    }

    /// Flush the trailing partial chunk (if any). Does not close the channel; the
    /// caller sends [`Msg::End`]/[`Msg::Fail`] explicitly afterward.
    fn flush_remaining(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            self.send_chunk()
        }
    }
}

impl Write for ChunkSink {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let total = data.len();
        while !data.is_empty() {
            let space = self.chunk_bytes - self.buf.len();
            let take = space.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() >= self.chunk_bytes {
                self.send_chunk()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        // A chunk boundary is a size decision, not a flush decision: the zstd
        // encoder flushes mid-frame and the receiver concatenates regardless, so
        // an intermediate flush must NOT emit a short chunk. The trailing partial
        // is pushed once, by `flush_remaining` at the end.
        Ok(())
    }
}

/// A producer-side closure that writes the full logical body to a sink: a
/// generic serde value (`to_writer_streaming`), a bulk typed numeric array
/// (`to_writer_typed_slice`), or a bulk complex array (`to_writer_complex_slice`).
/// Boxed so one producer engine serves every body shape.
type BodyWriter = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + Send>;

/// Map a beve encode error to an `io::Error` for the producer pipeline.
fn beve_to_io(e: beve::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// Run the (write→compress→chunk) pipeline to completion, reporting the outcome
/// through the channel. Always ends with exactly one `End` or `Fail` before the
/// channel closes.
fn produce(body: BodyWriter, tx: SyncSender<Msg>, opts: StreamOpts) {
    let mut sink = ChunkSink::new(tx.clone(), opts.chunk_bytes);
    let result = (|| -> io::Result<()> {
        match opts.compression {
            Compression::None => body(&mut sink)?,
            Compression::Zstd => {
                let mut enc = zstd::stream::write::Encoder::new(&mut sink, opts.zstd_level)?;
                body(&mut enc)?;
                enc.finish()?;
            }
        }
        sink.flush_remaining()
    })();
    // Send the terminal marker before `sink` (and its channel handle) drop, so the
    // consumer observes End/Fail in order rather than a bare channel close.
    let _ = match result {
        Ok(()) => tx.send(Msg::End),
        Err(e) => tx.send(Msg::Fail(e.to_string())),
    };
}

/// Per-stream consumer-facing state: the receiving end of the producer channel
/// plus the one-chunk lookahead used to mark the final chunk with `last = 1`.
struct Session {
    rx: Receiver<Msg>,
    lookahead: Option<Vec<u8>>,
    done: bool,
}

impl Session {
    fn recv(&self) -> Msg {
        self.rx.recv().unwrap_or_else(|_| {
            // The producer always sends End/Fail before dropping the sender, so a
            // bare close means the producer thread vanished (panic).
            Msg::Fail("svs producer ended without completing".to_string())
        })
    }

    /// Return the next chunk and whether it is the last. Blocks until the
    /// producer has the *following* chunk (or its terminal marker) ready — that
    /// block is the backpressure. Returns `Err` if the producer failed, so a
    /// truncated production never masquerades as a clean `last`.
    fn pull(&mut self) -> Result<(Vec<u8>, bool), String> {
        let current = match self.lookahead.take() {
            Some(c) => c,
            None => match self.recv() {
                Msg::Chunk(c) => c,
                Msg::End => return Ok((Vec::new(), true)),
                Msg::Fail(e) => return Err(e),
            },
        };
        match self.recv() {
            Msg::Chunk(next) => {
                self.lookahead = Some(next);
                Ok((current, false))
            }
            Msg::End => Ok((current, true)),
            Msg::Fail(e) => Err(e),
        }
    }
}

/// Server-side table of live download sessions, shared by the `open`/`next`/
/// `cancel` handlers. Each session is behind its own lock so a `next` can block
/// on production without holding the table lock.
struct SessionTable {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<u64, Arc<Mutex<Session>>>>,
}

impl SessionTable {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, stream_id: u64) -> Option<Arc<Mutex<Session>>> {
        self.sessions.lock().unwrap().get(&stream_id).cloned()
    }

    fn remove(&self, stream_id: u64) {
        self.sessions.lock().unwrap().remove(&stream_id);
    }
}

struct OpenHandler<F> {
    table: Arc<SessionTable>,
    make_body: F,
    opts: StreamOpts,
    /// The logical content's serialization format, reported in the `open`
    /// response so the consumer can reject an incompatible output. BEVE for the
    /// value/typed/complex producers, raw-binary for the opaque byte producer.
    format: u16,
}

impl<F> HandlerErased for OpenHandler<F>
where
    F: Fn(&str) -> Option<BodyWriter> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let open: OpenRequest = match beve_from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => {
                return Ok(error_like(
                    req,
                    ErrorCode::InvalidBody,
                    "svs open: expected BEVE { resource: string }",
                ));
            }
        };
        let Some(body) = (self.make_body)(&open.resource) else {
            return Ok(error_like(
                req,
                ErrorCode::MethodNotFound,
                format!("svs open: unknown resource '{}'", open.resource),
            ));
        };

        let stream_id = self.table.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel::<Msg>(self.opts.session_depth);
        let opts = self.opts;
        thread::spawn(move || produce(body, tx, opts));

        self.table.sessions.lock().unwrap().insert(
            stream_id,
            Arc::new(Mutex::new(Session {
                rx,
                lookahead: None,
                done: false,
            })),
        );

        let resp = OpenResponse {
            version: SVS_VERSION,
            stream_id,
            format: self.format,
            compression: self.opts.compression as u8,
        };
        beve_response(req, &resp)
    }
}

struct NextHandler {
    table: Arc<SessionTable>,
}

impl HandlerErased for NextHandler {
    fn execution(&self) -> Execution {
        // The pull blocks until the producer has the next chunk; on the WebSocket
        // server that must happen off the reader task so it stays free to decode a
        // concurrent `cancel`.
        Execution::OffReader
    }

    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let next: NextRequest = match beve_from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => {
                return Ok(error_like(
                    req,
                    ErrorCode::InvalidBody,
                    "svs next: expected BEVE { stream_id: u64 }",
                ));
            }
        };
        let Some(session) = self.table.get(next.stream_id) else {
            return Ok(error_like(
                req,
                ErrorCode::InvalidQuery,
                format!("svs next: unknown stream_id {}", next.stream_id),
            ));
        };

        // Pull under the per-session lock (not the table lock), then release it
        // before touching the table again to keep a single lock order.
        let outcome = {
            let mut guard = session.lock().unwrap();
            if guard.done {
                Err("svs next: stream already finished".to_string())
            } else {
                let pulled = guard.pull();
                if matches!(pulled, Ok((_, true)) | Err(_)) {
                    guard.done = true;
                }
                pulled
            }
        };

        match outcome {
            Ok((chunk, last)) => {
                if last {
                    self.table.remove(next.stream_id);
                }
                Ok(chunk_response(req, chunk, last))
            }
            Err(msg) => {
                self.table.remove(next.stream_id);
                Ok(error_like(req, ErrorCode::InternalError, msg))
            }
        }
    }
}

struct CancelHandler {
    table: Arc<SessionTable>,
}

impl HandlerErased for CancelHandler {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        // A malformed cancel still releases nothing but must not error a notify;
        // parse leniently and drop whatever stream_id we can read.
        if let Ok(cancel) = beve_from_slice::<CancelRequest>(&req.body) {
            self.table.remove(cancel.stream_id);
        }
        // Dropping the session drops the receiver, which aborts a parked producer
        // send. Acknowledge so a request-form cancel gets a reply (a notify-form
        // cancel ignores it).
        beve_response(req, &CancelAck { ok: true })
    }
}

#[derive(Serialize, Deserialize)]
struct CancelAck {
    ok: bool,
}

/// Register the three SVS routes onto `router` backed by `make_body`, the
/// producer-side factory mapping a resource to a [`BodyWriter`] (or `None`).
/// `format` is the logical content's serialization format reported in the `open`
/// response (BEVE for the serde/typed/complex producers, raw-binary for the
/// opaque byte producer).
fn register_svs<F>(router: Router, make_body: F, format: u16, opts: StreamOpts) -> Router
where
    F: Fn(&str) -> Option<BodyWriter> + Send + Sync + 'static,
{
    let table = Arc::new(SessionTable::new());
    let open: Arc<dyn HandlerErased> = Arc::new(OpenHandler {
        table: Arc::clone(&table),
        make_body,
        opts,
        format,
    });
    let next: Arc<dyn HandlerErased> = Arc::new(NextHandler {
        table: Arc::clone(&table),
    });
    let cancel: Arc<dyn HandlerErased> = Arc::new(CancelHandler {
        table: Arc::clone(&table),
    });
    router
        .with_erased_handler(ROUTE_OPEN, open)
        .with_erased_handler(ROUTE_NEXT, next)
        .with_erased_handler(ROUTE_CANCEL, cancel)
}

/// Extension trait that registers an SVS download producer onto a [`Router`].
///
/// Each registration installs the `/_svs/open`, `/_svs/next`, and `/_svs/cancel`
/// routes and serves **one** value shape; a router carries a single SVS producer
/// (a second registration replaces the routes). `resolve` maps an opaque resource
/// key (the `resource` field of the `open` request) to the value to stream, or
/// `None` if there is no such resource. The body is BEVE-encoded and, per
/// `opts.compression`, zstd-compressed as it is pulled; it is never copied whole.
///
/// See the [module docs](crate::value_stream) for where the producer may run
/// (sync or WebSocket server, not the inline async server).
pub trait RouterValueStreamExt: Sized {
    /// Stream an arbitrary `Serialize` value, decoded by the consumer with
    /// [`pull_value`] (generic, per-element serde).
    fn with_value_stream<T, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        T: Serialize + Send + 'static,
        F: Fn(&str) -> Option<T> + Send + Sync + 'static;

    /// Stream a bulk numeric array `Vec<T>` in BEVE typed-array form, so the
    /// consumer can decode it at memcpy speed with [`pull_typed_slice`]. Use this
    /// over [`with_value_stream`](Self::with_value_stream) for large primitive
    /// arrays (samples, matrices) where per-element serde is the bottleneck.
    fn with_typed_value_stream<T, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        T: beve::BeveTypedSlice + Send + 'static,
        F: Fn(&str) -> Option<Vec<T>> + Send + Sync + 'static;

    /// Stream a bulk complex array `Vec<Complex<T>>` in BEVE complex-array form,
    /// decoded by the consumer with [`pull_complex_slice`]. For large IQ buffers.
    fn with_complex_value_stream<T, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        T: beve::BeveTypedSlice + Send + 'static,
        F: Fn(&str) -> Option<Vec<Complex<T>>> + Send + Sync + 'static;

    /// Stream **opaque, already-serialized bytes** verbatim from a producer-side
    /// [`Read`] — no re-serialization. The stream is tagged
    /// `format = RawBinary`, so the consumer pulls it with [`pull_to_file`] /
    /// [`pull_to_file_async`] / [`pull_consume_async`] (the BEVE-typed pullers
    /// reject it).
    ///
    /// This is the right producer when the bytes already exist in their final
    /// on-disk form and must cross the wire unchanged: a file the server already
    /// wrote, a buffer it already encoded with its own serializer. With
    /// [`Compression::None`] the chunk stream is those exact bytes, so the
    /// consumer can write a byte-identical copy and verify it with its own
    /// digest. The element-type coupling of [`with_value_stream`] (the consumer
    /// must decode through *this* crate's serializer) does not apply.
    ///
    /// `resolve` maps a resource key to the byte source, or `None` if there is no
    /// such resource. Use `R = Box<dyn Read + Send>` to serve more than one
    /// concrete source type (e.g. a `File` for one resource, a `Cursor` for
    /// another) from a single registration.
    ///
    /// [`with_value_stream`]: Self::with_value_stream
    fn with_reader_stream<R, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        R: Read + Send + 'static,
        F: Fn(&str) -> Option<R> + Send + Sync + 'static;
}

impl RouterValueStreamExt for Router {
    fn with_value_stream<T, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        T: Serialize + Send + 'static,
        F: Fn(&str) -> Option<T> + Send + Sync + 'static,
    {
        register_svs(
            self,
            move |resource| {
                resolve(resource).map(|value| {
                    Box::new(move |w: &mut dyn Write| {
                        beve::to_writer_streaming(w, &value).map_err(beve_to_io)
                    }) as BodyWriter
                })
            },
            BodyFormat::Beve as u16,
            opts,
        )
    }

    fn with_typed_value_stream<T, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        T: beve::BeveTypedSlice + Send + 'static,
        F: Fn(&str) -> Option<Vec<T>> + Send + Sync + 'static,
    {
        register_svs(
            self,
            move |resource| {
                resolve(resource).map(|values| {
                    Box::new(move |w: &mut dyn Write| {
                        beve::to_writer_typed_slice(w, &values).map_err(beve_to_io)
                    }) as BodyWriter
                })
            },
            BodyFormat::Beve as u16,
            opts,
        )
    }

    fn with_complex_value_stream<T, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        T: beve::BeveTypedSlice + Send + 'static,
        F: Fn(&str) -> Option<Vec<Complex<T>>> + Send + Sync + 'static,
    {
        register_svs(
            self,
            move |resource| {
                resolve(resource).map(|values| {
                    Box::new(move |w: &mut dyn Write| {
                        beve::to_writer_complex_slice(w, &values).map_err(beve_to_io)
                    }) as BodyWriter
                })
            },
            BodyFormat::Beve as u16,
            opts,
        )
    }

    fn with_reader_stream<R, F>(self, resolve: F, opts: StreamOpts) -> Self
    where
        R: Read + Send + 'static,
        F: Fn(&str) -> Option<R> + Send + Sync + 'static,
    {
        register_svs(
            self,
            move |resource| {
                resolve(resource).map(|mut reader| {
                    Box::new(move |w: &mut dyn Write| io::copy(&mut reader, w).map(|_| ()))
                        as BodyWriter
                })
            },
            BodyFormat::RawBinary as u16,
            opts,
        )
    }
}

// ---- response framing helpers (server) ----------------------------------------

fn error_like(req: &Message, code: ErrorCode, msg: impl AsRef<str>) -> Message {
    let mut err = create_error_message(code, msg.as_ref());
    err.header.id = req.header.id;
    err
}

fn beve_response<T: Serialize>(req: &Message, value: &T) -> Result<Message, RepeError> {
    Ok(Message::builder()
        .id(req.header.id)
        .body_bytes(beve::to_vec(value)?)
        .body_format(BodyFormat::Beve)
        .build())
}

/// Frame a `next` response: the `last` flag rides in a 1-byte raw-binary query so
/// it never wraps the zero-copy chunk body, which stays raw binary.
fn chunk_response(req: &Message, chunk: Vec<u8>, last: bool) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_format(QueryFormat::RawBinary)
        .query_bytes(vec![last as u8])
        .body_format(BodyFormat::RawBinary)
        .body_bytes(chunk)
        .build()
}

// ---- client: pull driver ------------------------------------------------------

/// What the client does with the pulled byte stream (SVS §2: the receiver's
/// choice). The output must be consistent with the `format`/`compression` the
/// `open` response reports, or [`pull_stream`] cancels and errors rather than
/// producing a mislabeled file.
#[derive(Debug, Clone, Copy)]
pub enum StreamOutput<'a> {
    /// Write the chunk stream to a file as received. Requires a compressed stream
    /// (`compression = zstd`), since the bytes are the `.beve.zst` payload.
    BeveZstdFile(&'a Path),
    /// Decompress the chunk stream to a `.beve` file. Requires `compression =
    /// zstd` and `format = BEVE`.
    BeveFile(&'a Path),
    /// Stream-decode the value into a `T`. Requires `format = BEVE`; a compressed
    /// stream is decompressed on the way in. Returns `Some(T)`.
    Value,
    /// Write the logical content to a file, format-agnostic: a `compression =
    /// none` stream is copied verbatim (a byte-identical copy of an opaque
    /// [`with_reader_stream`](RouterValueStreamExt::with_reader_stream) source), a
    /// `compression = zstd` stream is decompressed on the way in. No `format`
    /// constraint, so it accepts a raw-binary stream the [`BeveFile`] mode would
    /// reject.
    ///
    /// [`BeveFile`]: StreamOutput::BeveFile
    RawFile(&'a Path),
}

/// Pull `resource` from an SVS producer reachable through `client` into `out`.
///
/// Returns `Ok(Some(T))` for [`StreamOutput::Value`] and `Ok(None)` for the file
/// outputs. File outputs are written to a temporary sibling path and renamed only
/// after the terminating `last = 1` chunk and a flush, so a dropped connection
/// never leaves a truncated file that looks complete (SVS §4).
pub fn pull_stream<T: DeserializeOwned>(
    client: &Client,
    resource: &str,
    out: StreamOutput<'_>,
) -> Result<Option<T>, RepeError> {
    let open = open_stream(client, resource)?;

    // Validate the chosen output against the returned tags before pulling a byte;
    // cancel the stream if we cannot honor them.
    if let Err(e) = check_output(out, &open) {
        let _ = cancel_stream(
            client,
            open.stream_id,
            "output incompatible with stream tags",
        );
        return Err(e);
    }

    match out {
        StreamOutput::BeveZstdFile(path) => {
            write_file(client, open.stream_id, path, |reader, file| {
                io::copy(reader, file)?;
                Ok(())
            })?;
            Ok(None)
        }
        StreamOutput::BeveFile(path) => {
            write_file(client, open.stream_id, path, |reader, file| {
                let mut dec = zstd::stream::read::Decoder::new(reader).map_err(RepeError::Io)?;
                io::copy(&mut dec, file)?;
                Ok(())
            })?;
            Ok(None)
        }
        StreamOutput::Value => {
            let reader = ChunkReader::new(client, open.stream_id);
            let value = match open.compression {
                Compression::None => beve::from_reader_streaming::<_, T>(reader)?,
                Compression::Zstd => {
                    let dec = zstd::stream::read::Decoder::new(reader).map_err(RepeError::Io)?;
                    beve::from_reader_streaming::<_, T>(dec)?
                }
            };
            // The value decoded fully; release any remainder of the stream the
            // decoder did not need to read. Best-effort.
            let _ = cancel_stream(client, open.stream_id, "value fully decoded");
            Ok(Some(value))
        }
        StreamOutput::RawFile(path) => {
            let compression = open.compression;
            write_file(client, open.stream_id, path, |reader, file| {
                match compression {
                    Compression::None => {
                        io::copy(reader, file)?;
                    }
                    Compression::Zstd => {
                        let mut dec =
                            zstd::stream::read::Decoder::new(reader).map_err(RepeError::Io)?;
                        io::copy(&mut dec, file)?;
                    }
                }
                Ok(())
            })?;
            Ok(None)
        }
    }
}

/// Convenience: pull `resource` and decode it into a `T` ([`StreamOutput::Value`]).
pub fn pull_value<T: DeserializeOwned>(client: &Client, resource: &str) -> Result<T, RepeError> {
    pull_stream::<T>(client, resource, StreamOutput::Value)
        .map(|v| v.expect("StreamOutput::Value always yields Some"))
}

/// Convenience: pull `resource` and write the compressed bytes to `path` as a
/// `.beve.zst` file ([`StreamOutput::BeveZstdFile`]).
pub fn pull_to_beve_zst_file(
    client: &Client,
    resource: &str,
    path: &Path,
) -> Result<(), RepeError> {
    pull_stream::<()>(client, resource, StreamOutput::BeveZstdFile(path)).map(|_| ())
}

/// Convenience: pull `resource`, decompress it, and write a `.beve` file
/// ([`StreamOutput::BeveFile`]).
pub fn pull_to_beve_file(client: &Client, resource: &str, path: &Path) -> Result<(), RepeError> {
    pull_stream::<()>(client, resource, StreamOutput::BeveFile(path)).map(|_| ())
}

/// Convenience: pull `resource` and write its logical content to `path`,
/// format-agnostic ([`StreamOutput::RawFile`]). For a `compression = none`
/// opaque byte stream this writes a byte-identical copy of the producer's source;
/// a compressed stream is decompressed first. The file is committed atomically
/// (temp sibling, renamed only after the terminating chunk and an fsync).
pub fn pull_to_file(client: &Client, resource: &str, path: &Path) -> Result<(), RepeError> {
    pull_stream::<()>(client, resource, StreamOutput::RawFile(path)).map(|_| ())
}

/// Pull `resource` as a bulk numeric array decoded straight into a `Vec<T>` at
/// memcpy speed (beve's `read_typed_slice_from_reader`), skipping the per-element
/// serde walk [`pull_value`] would do for a `Vec<T>`.
///
/// Pair with a server registered via
/// [`RouterValueStreamExt::with_typed_value_stream`], which emits the BEVE
/// typed-array wire form this reader requires. A compressed stream is decompressed
/// on the way in. Errors if the stream is not a typed array of `T` (e.g. a generic
/// `with_value_stream` producer, or a mismatched element type).
pub fn pull_typed_slice<T: beve::BeveTypedSlice>(
    client: &Client,
    resource: &str,
) -> Result<Vec<T>, RepeError> {
    let open = open_stream(client, resource)?;
    if let Err(e) = require_beve(&open) {
        let _ = cancel_stream(
            client,
            open.stream_id,
            "format incompatible with bulk decode",
        );
        return Err(e);
    }
    let reader = ChunkReader::new(client, open.stream_id);
    let result: Result<Vec<T>, RepeError> = match open.compression {
        Compression::None => {
            beve::read_typed_slice_from_reader::<T, _>(reader).map_err(RepeError::from)
        }
        Compression::Zstd => match zstd::stream::read::Decoder::new(reader) {
            Ok(dec) => beve::read_typed_slice_from_reader::<T, _>(dec).map_err(RepeError::from),
            Err(e) => Err(RepeError::Io(e)),
        },
    };
    // Release the stream whether or not the decode used every chunk (and on error).
    let _ = cancel_stream(client, open.stream_id, "bulk typed slice complete");
    result
}

/// Complex twin of [`pull_typed_slice`]: pull `resource` as a bulk
/// `Vec<Complex<T>>` (beve's `read_complex_slice_from_reader`). Pair with a server
/// registered via [`RouterValueStreamExt::with_complex_value_stream`]. For large
/// IQ buffers.
pub fn pull_complex_slice<T: beve::BeveTypedSlice>(
    client: &Client,
    resource: &str,
) -> Result<Vec<Complex<T>>, RepeError> {
    let open = open_stream(client, resource)?;
    if let Err(e) = require_beve(&open) {
        let _ = cancel_stream(
            client,
            open.stream_id,
            "format incompatible with bulk decode",
        );
        return Err(e);
    }
    let reader = ChunkReader::new(client, open.stream_id);
    let result: Result<Vec<Complex<T>>, RepeError> = match open.compression {
        Compression::None => {
            beve::read_complex_slice_from_reader::<T, _>(reader).map_err(RepeError::from)
        }
        Compression::Zstd => match zstd::stream::read::Decoder::new(reader) {
            Ok(dec) => beve::read_complex_slice_from_reader::<T, _>(dec).map_err(RepeError::from),
            Err(e) => Err(RepeError::Io(e)),
        },
    };
    let _ = cancel_stream(client, open.stream_id, "bulk complex slice complete");
    result
}

/// The bulk slice readers require the stream's logical format to be BEVE (the
/// typed/complex array is BEVE-encoded).
fn require_beve(open: &OpenInfo) -> Result<(), RepeError> {
    if open.format != BodyFormat::Beve as u16 {
        return Err(RepeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "svs: bulk slice decode needs format=BEVE, got format {}",
                open.format
            ),
        )));
    }
    Ok(())
}

struct OpenInfo {
    stream_id: u64,
    format: u16,
    compression: Compression,
}

/// BEVE body of an `open` request for `resource`.
fn open_request_body(resource: &str) -> Result<Vec<u8>, RepeError> {
    Ok(beve::to_vec(&OpenRequest {
        resource: resource.to_string(),
    })?)
}

/// BEVE body of a `next` request for `stream_id`.
fn next_request_body(stream_id: u64) -> Result<Vec<u8>, RepeError> {
    Ok(beve::to_vec(&NextRequest { stream_id })?)
}

/// BEVE body of a `cancel` request for `stream_id` carrying a human `reason`.
fn cancel_request_body(stream_id: u64, reason: &str) -> Result<Vec<u8>, RepeError> {
    Ok(beve::to_vec(&CancelRequest {
        stream_id,
        reason: reason.to_string(),
    })?)
}

/// Validate and parse an `open` response into the stream's [`OpenInfo`]. Shared
/// by the sync and async pull drivers.
fn parse_open_response(resp: &Message) -> Result<OpenInfo, RepeError> {
    let open: OpenResponse = resp.beve_body()?;
    if open.version != SVS_VERSION {
        return Err(RepeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "svs open: server contract version {} != supported {}",
                open.version, SVS_VERSION
            ),
        )));
    }
    let Some(compression) = Compression::from_u8(open.compression) else {
        return Err(RepeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("svs open: unknown compression tag {}", open.compression),
        )));
    };
    Ok(OpenInfo {
        stream_id: open.stream_id,
        format: open.format,
        compression,
    })
}

fn open_stream(client: &Client, resource: &str) -> Result<OpenInfo, RepeError> {
    let body = open_request_body(resource)?;
    let resp = client.call_with_formats(
        ROUTE_OPEN,
        QueryFormat::JsonPointer as u16,
        Some(&body),
        BodyFormat::Beve as u16,
    )?;
    parse_open_response(&resp)
}

fn check_output(out: StreamOutput<'_>, open: &OpenInfo) -> Result<(), RepeError> {
    let beve = BodyFormat::Beve as u16;
    let mismatch = |msg: String| {
        Err(RepeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            msg,
        )))
    };
    match out {
        StreamOutput::BeveZstdFile(_) => {
            if open.compression != Compression::Zstd {
                return mismatch(
                    "svs: .beve.zst output needs a zstd-compressed stream".to_string(),
                );
            }
        }
        StreamOutput::BeveFile(_) => {
            if open.compression != Compression::Zstd {
                return mismatch("svs: .beve output needs a zstd-compressed stream".to_string());
            }
            if open.format != beve {
                return mismatch(format!(
                    "svs: .beve output needs format=BEVE, got format {}",
                    open.format
                ));
            }
        }
        StreamOutput::Value => {
            if open.format != beve {
                return mismatch(format!(
                    "svs: value decode needs format=BEVE, got format {}",
                    open.format
                ));
            }
        }
        // Format-agnostic: any stream's logical content can land in a raw file.
        StreamOutput::RawFile(_) => {}
    }
    Ok(())
}

/// Drive the pull into a temp file via `fill`, then commit atomically: flush,
/// fsync, and rename onto `final_path` only after the stream terminated. The temp
/// file is removed if anything fails before the rename.
fn write_file<Fill>(
    client: &Client,
    stream_id: u64,
    final_path: &Path,
    fill: Fill,
) -> Result<(), RepeError>
where
    Fill: FnOnce(&mut ChunkReader<'_>, &mut std::fs::File) -> Result<(), RepeError>,
{
    let tmp_path = temp_sibling(final_path);
    let mut guard = TempFile::create(&tmp_path)?;
    let mut reader = ChunkReader::new(client, stream_id);

    let result = (|| {
        fill(&mut reader, guard.file_mut())?;
        if !reader.last_seen {
            // The fill loop returned without the terminating chunk — treat as a
            // truncated transfer rather than committing a short file.
            return Err(RepeError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "svs: stream ended without a final chunk",
            )));
        }
        guard.file_mut().flush()?;
        guard.file_mut().sync_all()?;
        Ok(())
    })();

    match result {
        Ok(()) => guard.commit(final_path),
        Err(e) => {
            // On failure, also tell the server to release the stream; the temp
            // file is removed by the guard's Drop.
            let _ = cancel_stream(client, stream_id, "receiver aborted");
            Err(e)
        }
    }
}

fn temp_sibling(final_path: &Path) -> PathBuf {
    let mut name = final_path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".svspart");
    final_path.with_file_name(name)
}

/// Owns a temp file and removes it on drop unless [`commit`](TempFile::commit)
/// renamed it into place first.
struct TempFile {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl TempFile {
    fn create(path: &Path) -> Result<Self, RepeError> {
        let file = std::fs::File::create(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
        })
    }

    fn file_mut(&mut self) -> &mut std::fs::File {
        self.file.as_mut().expect("temp file open until commit")
    }

    fn commit(mut self, final_path: &Path) -> Result<(), RepeError> {
        self.file = None; // close before rename
        std::fs::rename(&self.path, final_path)?;
        self.path = final_path.to_path_buf(); // committed; Drop must not remove it
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.file.is_some() {
            // Not committed — close and remove the partial temp file.
            self.file = None;
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A `Read` adapter that issues `next` requests on demand and surfaces the
/// terminating `last = 1` as EOF. Modes that need a decoder wrap this; the raw
/// file mode reads it directly.
struct ChunkReader<'a> {
    client: &'a Client,
    stream_id: u64,
    buf: Vec<u8>,
    pos: usize,
    finished: bool,
    last_seen: bool,
}

impl<'a> ChunkReader<'a> {
    fn new(client: &'a Client, stream_id: u64) -> Self {
        Self {
            client,
            stream_id,
            buf: Vec::new(),
            pos: 0,
            finished: false,
            last_seen: false,
        }
    }

    fn fetch(&mut self) -> io::Result<()> {
        let body = beve::to_vec(&NextRequest {
            stream_id: self.stream_id,
        })
        .map_err(|e| io::Error::other(e.to_string()))?;
        let resp = self
            .client
            .call_with_formats(
                ROUTE_NEXT,
                QueryFormat::JsonPointer as u16,
                Some(&body),
                BodyFormat::Beve as u16,
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
        let last = resp.query.first().copied() == Some(1);
        self.buf = resp.body;
        self.pos = 0;
        if last {
            self.last_seen = true;
            self.finished = true;
        }
        Ok(())
    }
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = out.len().min(self.buf.len() - self.pos);
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.finished {
                return Ok(0);
            }
            self.fetch()?;
            // After a fetch the buffer may be empty (a final empty `last` chunk);
            // loop once more, where `finished` now short-circuits to EOF.
        }
    }
}

fn cancel_stream(client: &Client, stream_id: u64, reason: &str) -> Result<(), RepeError> {
    let body = cancel_request_body(stream_id, reason)?;
    // Notify form: best-effort, no reply awaited (SVS §2.3).
    client.notify_with_formats(
        ROUTE_CANCEL,
        QueryFormat::JsonPointer as u16,
        Some(&body),
        BodyFormat::Beve as u16,
    )
}

// ---- client: async pull driver (over `AsyncClient`) ---------------------------
//
// The SVS receiver is a blocking pull loop feeding a blocking decoder
// (`beve::from_reader_streaming` / the bulk slice readers / `zstd`'s `Read`
// decoder) — none of which are `async`. To drive a pull from async code without
// blocking the runtime, we split the two halves across the async/blocking
// boundary: an async task issues `next` requests and feeds each chunk into a
// bounded channel, while a `spawn_blocking` task runs the decoder over a blocking
// `Read` that drains that channel. The bound supplies backpressure (the SVS
// round trip itself), so neither side runs ahead of the other.

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::async_client::AsyncClient {}
    #[cfg(feature = "websocket")]
    impl Sealed for crate::websocket_client::WebSocketClient {}
}

/// The async REPE transport an SVS pull is driven over. Implemented for the
/// async-TCP [`AsyncClient`] and, with the `websocket` feature, the
/// [`WebSocketClient`](crate::WebSocketClient) — both expose the same
/// request/notify-with-formats calls the pull loop needs, so the async pullers
/// accept either. Sealed: it abstracts over the in-crate async clients and is not
/// meant to be implemented downstream.
//
// `async fn` in a public trait normally warns (callers can't name/bound the
// returned future), but every use here `.await`s it inline on the caller's task
// — never spawned — so no `Send` bound is needed and the warning does not apply.
#[allow(async_fn_in_trait)]
pub trait AsyncSvsClient: sealed::Sealed {
    /// Request/response with explicit query/body formats (drives `open` / `next`).
    async fn svs_call(
        &self,
        path: &str,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<Message, RepeError>;

    /// Fire-and-forget notify with explicit formats (drives best-effort `cancel`).
    async fn svs_notify(
        &self,
        path: &str,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError>;
}

impl AsyncSvsClient for AsyncClient {
    async fn svs_call(
        &self,
        path: &str,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<Message, RepeError> {
        self.call_with_formats(path, query_format, body, body_format)
            .await
    }

    async fn svs_notify(
        &self,
        path: &str,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError> {
        self.notify_with_formats(path, query_format, body, body_format)
            .await
    }
}

#[cfg(feature = "websocket")]
impl AsyncSvsClient for crate::websocket_client::WebSocketClient {
    async fn svs_call(
        &self,
        path: &str,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<Message, RepeError> {
        self.call_with_formats(path, query_format, body, body_format)
            .await
    }

    async fn svs_notify(
        &self,
        path: &str,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError> {
        self.notify_with_formats(path, query_format, body, body_format)
            .await
    }
}

/// Chunks held in flight between the async pull task and the blocking decoder.
/// Small: the SVS round trip already paces the producer, so this only needs to
/// cover the gap between a `next` completing and the decoder consuming it.
const ASYNC_PULL_DEPTH: usize = 4;

/// A blocking [`Read`] over a channel of chunks. Lives on the `spawn_blocking`
/// thread and is fed by [`pull_loop_async`]; `blocking_recv` parks that thread
/// (never the runtime) until the next chunk or end-of-stream arrives.
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = out.len().min(self.buf.len() - self.pos);
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.blocking_recv() {
                Some(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                // Sender dropped: the pull loop reached `last` (or errored). The
                // decoder sees a clean EOF; a short read surfaces as its own error.
                None => return Ok(0),
            }
        }
    }
}

/// Issue `next` requests over `client` and feed each non-empty chunk into `tx`
/// until the `last` flag. Returns early (without error) if the decoder finished
/// and dropped its receiver. `tx` drops on return, signalling EOF to the reader.
async fn pull_loop_async<C: AsyncSvsClient>(
    client: &C,
    stream_id: u64,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<(), RepeError> {
    let body = next_request_body(stream_id)?;
    loop {
        let resp = client
            .svs_call(
                ROUTE_NEXT,
                QueryFormat::JsonPointer as u16,
                Some(&body),
                BodyFormat::Beve as u16,
            )
            .await?;
        let last = resp.query.first().copied() == Some(1);
        if !resp.body.is_empty() && tx.send(resp.body).await.is_err() {
            // Decoder is done and dropped the receiver; nothing left to feed.
            return Ok(());
        }
        if last {
            return Ok(());
        }
    }
}

/// Best-effort async `cancel` (notify form, no reply awaited; SVS §2.3).
async fn cancel_stream_async<C: AsyncSvsClient>(
    client: &C,
    stream_id: u64,
    reason: &str,
) -> Result<(), RepeError> {
    let body = cancel_request_body(stream_id, reason)?;
    client
        .svs_notify(
            ROUTE_CANCEL,
            QueryFormat::JsonPointer as u16,
            Some(&body),
            BodyFormat::Beve as u16,
        )
        .await
}

/// Send the `open` request over an async client and parse the response into an
/// [`OpenInfo`]. Shared by every async puller; the per-puller format check (if
/// any) runs on the returned `OpenInfo`.
async fn open_async<C: AsyncSvsClient>(client: &C, resource: &str) -> Result<OpenInfo, RepeError> {
    let body = open_request_body(resource)?;
    let resp = client
        .svs_call(
            ROUTE_OPEN,
            QueryFormat::JsonPointer as u16,
            Some(&body),
            BodyFormat::Beve as u16,
        )
        .await?;
    parse_open_response(&resp)
}

/// Pull an already-opened stream and consume it: the async pull loop and the
/// blocking `consume` run concurrently across the channel, with `consume`
/// receiving a `Read` over the (optionally zstd-decompressed) chunk stream.
///
/// A pull/network error is surfaced *before* `consume`'s value, so a truncated
/// transfer never returns a value built from a short read: the value (and
/// anything it owns, e.g. an uncommitted temp file) is dropped on that path.
async fn run_pull<V, F, C: AsyncSvsClient>(
    client: &C,
    open: OpenInfo,
    consume: F,
) -> Result<V, RepeError>
where
    V: Send + 'static,
    F: FnOnce(Box<dyn Read>) -> Result<V, RepeError> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(ASYNC_PULL_DEPTH);
    let compression = open.compression;
    // Starts immediately on a blocking thread; runs concurrently with the pull
    // loop below, the two paced by the channel bound.
    let consume_task = tokio::task::spawn_blocking(move || -> Result<V, RepeError> {
        let reader: Box<dyn Read> = match compression {
            Compression::None => Box::new(ChannelReader::new(rx)),
            Compression::Zstd => Box::new(
                zstd::stream::read::Decoder::new(ChannelReader::new(rx)).map_err(RepeError::Io)?,
            ),
        };
        consume(reader)
    });

    let pull_res = pull_loop_async(client, open.stream_id, tx).await;
    let consume_res = consume_task.await;
    // Best-effort release; the stream is spent (or the pull failed) either way.
    let _ = cancel_stream_async(client, open.stream_id, "pull complete").await;

    // A pull/network error explains any consume-side EOF, so surface it first.
    pull_res?;
    match consume_res {
        Ok(v) => v,
        Err(join_err) => Err(RepeError::Io(io::Error::other(format!(
            "svs consume task failed: {join_err}"
        )))),
    }
}

/// Open `resource`, require it be BEVE, and `run_pull` it. Shared by the async
/// value and bulk-slice pullers (which all decode BEVE).
async fn pull_beve_async<V, F, C: AsyncSvsClient>(
    client: &C,
    resource: &str,
    decode: F,
) -> Result<V, RepeError>
where
    V: Send + 'static,
    F: FnOnce(Box<dyn Read>) -> Result<V, RepeError> + Send + 'static,
{
    let open = open_async(client, resource).await?;
    if let Err(e) = require_beve(&open) {
        let _ = cancel_stream_async(client, open.stream_id, "format incompatible").await;
        return Err(e);
    }
    run_pull(client, open, decode).await
}

/// Async counterpart of [`pull_value`]: pull `resource` and decode the whole
/// stream into a `T` via BEVE (streaming, optionally zstd-decompressed).
///
/// `client` is any [`AsyncSvsClient`] — the async-TCP [`AsyncClient`] or, with
/// the `websocket` feature, the [`WebSocketClient`](crate::WebSocketClient). The
/// producer must run on a server that honours
/// [`Execution::OffReader`](crate::Execution) (the sync [`Server`](crate::Server)
/// or the WebSocket server), not the inline async server.
pub async fn pull_value_async<T: DeserializeOwned + Send + 'static, C: AsyncSvsClient>(
    client: &C,
    resource: &str,
) -> Result<T, RepeError> {
    pull_beve_async(client, resource, |reader| {
        beve::from_reader_streaming::<_, T>(reader).map_err(RepeError::from)
    })
    .await
}

/// Async counterpart of [`pull_typed_slice`]: bulk-decode a streamed numeric
/// array straight into a `Vec<T>` (memcpy of the little-endian payload). Accepts
/// any [`AsyncSvsClient`].
pub async fn pull_typed_slice_async<T: beve::BeveTypedSlice + Send + 'static, C: AsyncSvsClient>(
    client: &C,
    resource: &str,
) -> Result<Vec<T>, RepeError> {
    pull_beve_async(client, resource, |reader| {
        beve::read_typed_slice_from_reader::<T, _>(reader).map_err(RepeError::from)
    })
    .await
}

/// Async counterpart of [`pull_complex_slice`]: bulk-decode a streamed complex
/// array straight into a `Vec<Complex<T>>`. Accepts any [`AsyncSvsClient`].
pub async fn pull_complex_slice_async<
    T: beve::BeveTypedSlice + Send + 'static,
    C: AsyncSvsClient,
>(
    client: &C,
    resource: &str,
) -> Result<Vec<Complex<T>>, RepeError> {
    pull_beve_async(client, resource, |reader| {
        beve::read_complex_slice_from_reader::<T, _>(reader).map_err(RepeError::from)
    })
    .await
}

/// Async, format-agnostic escape hatch: pull `resource` and hand its logical
/// content to `consume` as a blocking [`Read`] (a `compression = zstd` stream is
/// decompressed on the way in; `none` is delivered verbatim). Unlike
/// [`pull_value_async`] this places no `format` constraint, so it drives an
/// opaque [`with_reader_stream`](RouterValueStreamExt::with_reader_stream)
/// producer as well as a BEVE one.
///
/// `consume` runs on a `spawn_blocking` thread and may block freely. The value it
/// returns is delivered only if the pull completed cleanly; on a truncated /
/// dropped transfer this returns the pull error and the value is dropped (so a
/// temp file `consume` opened is discarded rather than committed — see
/// [`pull_to_file_async`] / [`pull_to_file_verified_async`], which build on this).
pub async fn pull_consume_async<V, F, C: AsyncSvsClient>(
    client: &C,
    resource: &str,
    consume: F,
) -> Result<V, RepeError>
where
    V: Send + 'static,
    F: FnOnce(Box<dyn Read>) -> Result<V, RepeError> + Send + 'static,
{
    let open = open_async(client, resource).await?;
    run_pull(client, open, consume).await
}

/// A [`Write`] that forwards every byte to two sinks. Used to write a file while
/// feeding the same bytes to a digest in a single pass.
struct TeeWriter<'a, A: Write, B: Write> {
    file: &'a mut A,
    digest: &'a mut B,
}

impl<A: Write, B: Write> Write for TeeWriter<'_, A, B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write_all(buf)?;
        self.digest.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.digest.flush()
    }
}

/// Async counterpart of [`pull_to_file`]: pull `resource` and write its logical
/// content to `path`, committed atomically (temp sibling, renamed only after the
/// terminating chunk and an fsync). A `compression = none` opaque byte stream is
/// written verbatim; a compressed stream is decompressed first. Returns the
/// number of content bytes written.
///
/// To verify content integrity before the file is published, use
/// [`pull_to_file_verified_async`].
pub async fn pull_to_file_async<C: AsyncSvsClient>(
    client: &C,
    resource: &str,
    path: &Path,
) -> Result<u64, RepeError> {
    let tmp_path = temp_sibling(path);
    let (guard, bytes) = pull_consume_async(client, resource, move |mut reader| {
        let mut guard = TempFile::create(&tmp_path)?;
        let bytes = io::copy(&mut reader, guard.file_mut())?;
        guard.file_mut().flush()?;
        guard.file_mut().sync_all()?;
        Ok((guard, bytes))
    })
    .await?;
    guard.commit(path)?;
    Ok(bytes)
}

/// Async file pull with a caller-supplied integrity gate. As content bytes are
/// written to the temp file they are also fed to `digest` (any [`Write`] — a
/// hasher, a CRC accumulator); after the stream completes cleanly and before the
/// temp file is renamed into place, `verify(digest)` is called. If it returns
/// `Err`, the temp file is discarded and nothing is committed at `path`; if it
/// returns `Ok`, the file is renamed in atomically.
///
/// This is the integrity seam SVS itself does not commit on the wire: the
/// producer and consumer agree on a content digest out of band (e.g. carried in
/// an application manifest), the consumer recomputes it here in one pass over the
/// streamed bytes, and a mismatch vetoes publication. A truncated transfer is
/// caught upstream of `verify` (it surfaces as the pull error) and likewise
/// commits nothing.
pub async fn pull_to_file_verified_async<C, D, V>(
    client: &C,
    resource: &str,
    path: &Path,
    digest: D,
    verify: V,
) -> Result<(), RepeError>
where
    C: AsyncSvsClient,
    D: Write + Send + 'static,
    V: FnOnce(D) -> Result<(), RepeError>,
{
    let tmp_path = temp_sibling(path);
    let (guard, digest) = pull_consume_async(client, resource, move |mut reader| {
        let mut guard = TempFile::create(&tmp_path)?;
        let mut digest = digest;
        {
            let mut tee = TeeWriter {
                file: guard.file_mut(),
                digest: &mut digest,
            };
            io::copy(&mut reader, &mut tee)?;
        }
        guard.file_mut().flush()?;
        guard.file_mut().sync_all()?;
        Ok((guard, digest))
    })
    .await?;
    // The stream terminated cleanly (a truncation would have surfaced as the pull
    // error above, before this point, discarding `guard`). Let the caller accept
    // or reject the content; only an accept commits the file.
    verify(digest)?;
    guard.commit(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &Receiver<Msg>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Chunk(c) => out.push(c),
                Msg::End | Msg::Fail(_) => break,
            }
        }
        out
    }

    #[test]
    fn chunk_sink_splits_on_size_and_flushes_remainder() {
        let (tx, rx) = sync_channel::<Msg>(64);
        let mut sink = ChunkSink::new(tx, 4);
        // 10 bytes at chunk size 4 -> [4][4][2].
        sink.write_all(&[0u8; 10]).unwrap();
        sink.flush_remaining().unwrap();
        drop(sink);
        let chunks = drain(&rx);
        assert_eq!(
            chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
            vec![4, 4, 2]
        );
        assert_eq!(chunks.concat().len(), 10);
    }

    #[test]
    fn chunk_sink_intermediate_flush_does_not_emit_short_chunk() {
        let (tx, rx) = sync_channel::<Msg>(64);
        let mut sink = ChunkSink::new(tx, 8);
        sink.write_all(&[1u8; 3]).unwrap();
        sink.flush().unwrap(); // must not push a 3-byte chunk
        sink.write_all(&[2u8; 7]).unwrap(); // now 10 buffered -> one 8-chunk emitted
        sink.flush_remaining().unwrap();
        drop(sink);
        let chunks = drain(&rx);
        assert_eq!(
            chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
            vec![8, 2]
        );
    }

    fn session_from(msgs: Vec<Msg>) -> Session {
        let (tx, rx) = sync_channel::<Msg>(msgs.len().max(1));
        for m in msgs {
            tx.send(m).unwrap();
        }
        drop(tx);
        Session {
            rx,
            lookahead: None,
            done: false,
        }
    }

    #[test]
    fn pull_marks_only_the_final_chunk_last() {
        let mut s = session_from(vec![
            Msg::Chunk(vec![1]),
            Msg::Chunk(vec![2]),
            Msg::Chunk(vec![3]),
            Msg::End,
        ]);
        assert_eq!(s.pull().unwrap(), (vec![1], false));
        assert_eq!(s.pull().unwrap(), (vec![2], false));
        assert_eq!(s.pull().unwrap(), (vec![3], true));
    }

    #[test]
    fn pull_single_chunk_is_last() {
        let mut s = session_from(vec![Msg::Chunk(vec![9, 9]), Msg::End]);
        assert_eq!(s.pull().unwrap(), (vec![9, 9], true));
    }

    #[test]
    fn pull_zero_chunk_stream_is_empty_last() {
        let mut s = session_from(vec![Msg::End]);
        assert_eq!(s.pull().unwrap(), (Vec::<u8>::new(), true));
    }

    #[test]
    fn pull_surfaces_producer_failure_instead_of_clean_end() {
        // The lookahead peek encounters the failure before the buffered chunk
        // could be mislabeled `last`, so the failure surfaces as an error rather
        // than a clean terminus — the buffered chunk is discarded, not delivered.
        let mut s = session_from(vec![Msg::Chunk(vec![1]), Msg::Fail("boom".into())]);
        assert_eq!(s.pull().unwrap_err(), "boom");
    }

    #[test]
    fn pull_dropped_producer_is_an_error_not_a_terminus() {
        let (tx, rx) = sync_channel::<Msg>(1);
        tx.send(Msg::Chunk(vec![7])).unwrap();
        drop(tx); // vanish without End/Fail
        let mut s = Session {
            rx,
            lookahead: None,
            done: false,
        };
        // First pull yields the buffered chunk, peek sees the bare close -> error.
        assert!(s.pull().is_err());
    }
}
