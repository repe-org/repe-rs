# Changelog

## [3.6.0] - 2026-06-09

### Added
- A bulk numeric route that carries the `body_typed_slice` / `decode_typed_slice` fast path end-to-end through the high-level client and server, so large contiguous `f32`/`f64` (and other scalar) arrays no longer pay serde's per-element walk on the request and response. `Router::with_slice::<T, R, _>(path, |Vec<T>| -> Result<Vec<R>, _>)` decodes the request body with one bounds-checked bulk copy (`beve::read_typed_slice`) and frames the `Vec<R>` response with one bulk write (`MessageBuilder::body_typed_slice`), implementing both the owned and the allocation-free borrowing (`handle_view`) dispatch paths. `AsyncClient::call_slice` / `Client::call_slice` (with `_with_timeout` variants) are the client counterpart: they send a `&[T]` body via the bulk writer and decode the `Vec<R>` response via the bulk reader. The wire bytes are byte-for-byte identical to the serde (`with_typed` / `call_typed_beve`) path, so a `with_slice` route and a serde client (or vice versa) interoperate freely; only the encode side is changed, and only where the element type is statically a `BeveTypedSlice` scalar. A request whose body is not a BEVE typed array of `T` is rejected with `ErrorCode::InvalidBody`; a typed array of the wrong element class/width surfaces as `RepeError::Beve` rather than being misread. Measured end-to-end against gRPC+protobuf over loopback, this turns the large-`f64` case from a ~2x loss into a 6-8x win (e.g. 1M `f64` echo: 47.6 ms → 5.0 ms), and widens the integer-array win further (1M `i64`: 40.9 ms → 5.2 ms). See `tests/slice_fastpath.rs`. Zero-copy borrow of the body is intentionally not offered: the numeric payload follows a 1-byte tag and a varint length, so it is not `T`-aligned on the wire, and the single aligned `copy_nonoverlapping` is the floor.

### Changed
- The async server (`AsyncServer::serve`) now sets `TCP_NODELAY` on every accepted connection, matching what `AsyncClient::connect` already does on its side and what the synchronous `Server` already does by default (its configurable `tcp_nodelay`, default on). A buffered response is flushed as a single write, so coalescing it with delayed-ACK only added latency, most visibly on large multi-segment numeric bodies. Like `AsyncClient`, the async server enables it unconditionally with no opt-out (the synchronous `Server`'s `tcp_nodelay` toggle is a separate lineage); there is no known reason to want Nagle on this path. A `set_nodelay` failure is non-fatal and left unpropagated (the socket still works, just with Nagle on).

## [3.5.2] - 2026-06-09

### Added
- Cross-language wire-compatibility test suite against the canonical C++ REPE implementation (Glaze). A committed C++ generator (`interop/cpp/`) links Glaze and emits authentic REPE v1 frames; those bytes and a manifest live under `interop/fixtures/`; `tests/interop.rs` asserts parse parity, body decode (JSON / BEVE object / BEVE typed numeric / UTF-8 error), byte-identity round-trip, and — for protocol-defined layouts — from-scratch encoder parity. Notably, repe-rs's `body_typed_slice` BEVE numeric arrays and its error frames are byte-identical to Glaze's. A gated `interop` CI workflow rebuilds the generator from the pinned Glaze tag, regenerates, and fails on drift. No library code or public API changed; `interop/` is excluded from the published crate. See `docs/interop.md`. Both implementations are REPE v1 (48-byte header), the shipping spec; a v2 (32-byte header) exists only as an unreleased work-in-progress branch of the spec, so adopting it is separate future work.

## [3.5.1] - 2026-06-09

### Changed
- `MessageBuilder::body_typed_slice` / `body_complex_slice` now allocate their body buffer with `HEADER_SIZE + query.len()` of spare capacity reserved after the encoded payload, so shipping the built message through `Message::into_wire_bytes` reuses that allocation in place instead of allocating a fresh frame buffer. The encode itself stays a single allocation (the slice is written into the pre-sized buffer via `beve::to_writer_*_slice` rather than `to_vec_*_slice`), and the wire bytes are unchanged. The headroom is effective whenever the body is encoded with the final query already in place — the common `.query_*(..).body_typed_slice(..)` order, and also the query-less case (the reserved prefix is then just the header). Only setting a non-empty query *after* the body misses it, falling back to a fresh frame exactly as before. Only affects the build-a-Message-then-`into_wire_bytes` outbound pattern; the streaming `write_message_typed_slice` / `write_message_complex_slice` path was already zero-buffer.

## [3.5.0] - 2026-06-09

### Added
- `write_message_complex_slice`, the complex counterpart of `write_message_typed_slice`: it frames a REPE message whose entire body is a complex numeric array (`&[Complex<T>]`) straight to a sink with no intermediate body buffer, sizing it in closed form (`beve::complex_slice_size`, O(1)) and writing the interleaved `(re, im)` payload in one bulk write. The wire bytes are identical to a `MessageBuilder::body_complex_slice` message framed with `write_message`; decode with `Message::decode_complex_slice`. This closes the streaming-framing gap left in 3.4.0, where complex bodies had no zero-buffer path and had to be built and allocated whole via `body_complex_slice`.

### Changed
- Depend on `beve = "2.1"` (raised from `"2"`), which adds the `to_writer_complex_slice` / `complex_slice_size` streaming primitives backing the new framing helper.

## [3.4.0] - 2026-06-08

### Added
- A whole-body fast path for high-throughput numeric payloads (typed numeric and complex arrays), bypassing serde's per-element walk. `MessageBuilder::body_typed_slice(&[T])` / `body_complex_slice(&[Complex<T>])` encode a contiguous slice as a BEVE typed/complex array in one bulk write; `Message::decode_typed_slice::<T>()` / `decode_complex_slice::<T>()` read it back in a single bounds-checked bulk copy. The bytes are byte-for-byte identical to the serde `body_beve(&Vec<T>)` path, so the bulk and serde paths interoperate; the decoders also read serde-produced bodies and vice versa. `write_message_typed_slice` frames a numeric body straight to a sink with no intermediate body buffer, sizing it in closed form (`beve::typed_slice_size`, O(1)) and writing the payload in one bulk write. New `RepeError::UnexpectedBodyFormat { expected, got }` (maps to `ErrorCode::InvalidBody`) reported by the decoders when the body is not `BodyFormat::Beve`. `beve::{BeveTypedSlice, Complex}` re-exported at the crate root. See `docs/numeric-bodies.md` and `examples/typed_numeric_body.rs`.
- `benches/wire_serialization.rs` gains `typed_numeric_framing_f64`, comparing serde streaming framing against the typed-slice fast path across body sizes (~14x at 64 elements rising to ~25-33x once memory-bandwidth bound).

### Changed
- Depend on `beve = "2"` (raised from 1.4) and drop `default-features = false`. beve 2.0's default feature set is already lean -- its heavy MATLAB/HDF5 `mat` interop is now opt-in -- so repe no longer needs to disable default features, and 2.0 carries the `read_typed_slice` / `read_complex_slice` bulk decoders backing the new numeric body path.
- `Message::json_body` and `beve_body` now report a wrong-format body as `RepeError::UnexpectedBodyFormat` (→ `ErrorCode::InvalidBody`), the same structured error the new bulk decoders use, instead of synthesizing a `serde_json` / BEVE error tagged `ParseError`. One error shape for "wrong body format" across all four body decoders. Code matching on the previous `RepeError::Json(_)` / `RepeError::Beve(_)` for that specific case should match `UnexpectedBodyFormat`; a successful decode is unaffected.

## [3.3.0] - 2026-05-30

### Added
- Borrowing, zero-allocation read path for server dispatch. `read_message_into(reader, &mut Vec<u8>)` and `read_message_into_async` read a full frame into a reusable per-connection buffer; pair with `MessageView::from_slice`/`from_slice_exact` (the latter rejects trailing bytes, for one-message-per-frame transports) to dispatch via the new `HandlerErased::handle_view(&MessageView, &CallContext)` without the per-request query and body `Vec` allocations an owned `Message` requires. `handle_view`'s default materializes an owned `Message` (`MessageView::to_message`) and delegates to `handle_with_ctx`, so every existing handler works unchanged; the built-in JSON and typed handlers (`with_json` / `with_typed`) override it to decode straight from the borrowed body.
- A measurement harness for the server hot path: `tests/allocations.rs` pins the per-request allocation count of read → dispatch → echo → write with a thread-local counting `#[global_allocator]` and `assert_eq` budgets (so a regression up *or* a deliberate reduction down is caught), and `benches/server_lifecycle.rs` measures the same cycle end-to-end.

### Changed
- The TCP, async, and WebSocket-inline servers now dispatch off the borrowing read path. The TCP and async servers keep one reusable read buffer per connection and frame the response echoing the query straight out of it, so steady-state request framing for `with_json`/`with_typed` routes allocates nothing (per-request allocations 5 → 3; `server_lifecycle` `json_echo` ~341→316 ns, `typed_sum` ~176→137 ns). The WebSocket inline path borrows the request from the tungstenite payload, eliminating the request body copy (5 → 4; the response query is still copied because the outbound channel carries owned messages framed later by the writer task); the WebSocket off-reader path keeps the owned decode (its spawned task outlives the read buffer).
- Per-response query echo is now a buffer move (owned/WebSocket path) or a borrow (TCP/async), not a clone. Built-in handler responses leave the query empty and the dispatch layer supplies it; a handler that sets its own response query is left untouched. Registry JSON-pointer resolution borrows escape-free tokens (`Cow`) and builds error-path strings lazily, taking a depth-N pointer walk from ~`2N` `String` allocations to zero (`registry_value` read −33%, write −22%). Owned JSON/typed decoders parse a `Utf8`-framed body with strict `from_slice` (matching the borrowing path; drops a per-request `String`).

### Notes
- The 5→3 / "framing allocates nothing" win applies to `with_json`/`with_typed` routes, which override `handle_view`. Context-aware, struct, registry, and middleware-wrapped routes use the owning `handle_view` default (a correct owned copy) and keep the owned allocation count until overridden; middleware-wrapped routes are structurally bound to the owned path because the `Middleware` trait takes `&Message`.
- Behavior change to a public trait method, documented on `HandlerErased`: built-in handlers' `handle` / `handle_with_ctx` now return a response whose `query` is *empty* — the dispatch layer fills it. Code that calls a handler directly (e.g. via `Router::get`) and needs a complete, query-echoing response should build it with `create_response`, which echoes the query itself. The owned/WebSocket error path and success frames are byte-identical to before. (One edge improvement: a `Utf8`-framed body containing invalid UTF-8 is now rejected strictly rather than lossily substituted.)

## [3.2.0] - 2026-05-29

### Added
- One-port co-hosting classifier: `is_websocket_upgrade(&TcpStream) -> io::Result<bool>` (re-exported at the crate root). A non-destructive WS-vs-HTTP sniff that lets a single accept loop fork REPE WebSocket upgrades to `SharedWebSocketServer::serve_connection` and everything else to the embedder's own HTTP handler on one TCP port. It uses `TcpStream::peek`, so the request bytes stay in the socket buffer for the subsequent `WebSocketServer::accept` (which replays the handshake from the start of the stream); it matches the `websocket` token only inside an `Upgrade` header's value, so `GET /websocket-status` is not misclassified, and `accept` still performs the authoritative RFC 6455 validation on the `true` branch.
- `examples/websocket_cohosting.rs`: a runnable end-to-end demo that fills in both halves of the co-hosting pattern — the classifier plus a minimal, dependency-free `serve_http` (`/healthz` plus a JSON `GET`) — driving both forks against one port. Linked from the co-hosting section of `docs/websocket.md`.
- `PeerRegistry` alias index, for an embedder whose client identity is its own key (a UUID, a token) rather than the server-minted `PeerId`: `alias(peer_id, key)`, `get_by(&key)` (generic borrowed lookup like `HashMap::get`), `key_for(peer_id)`, and `aliases_for(peer_id)`. `PeerId` stays canonical, so `with_peer_registry` auto-insert is unchanged. A reverse index lets `remove` purge a peer's aliases without scanning the forward map, so disconnect cleanup stays automatic; `key_for` / `aliases_for` map a `PeerId` from a `broadcast_notify_*` result map back to the embedder's identity. Replaces hand-maintaining a parallel `key -> PeerId` map.
- `HandshakeContext` and `WebSocketServer::on_peer_connect_with_handshake(f)` (both re-exported at the crate root): a connect hook that also receives the upgrade request's `path()`, `query()`, and `header(name)`, so an embedder whose key rides in the handshake can derive it and `alias` the freshly-inserted peer at connect time. It fires *after* the plain `on_peer_connect` hooks, so a `with_peer_registry` insert has already landed and the peer is present for `alias`. The context is threaded through the built-in `serve*` loops and the new co-hosting methods `WebSocketServer::accept_with_handshake`, `SharedWebSocketServer::serve_connection_with_handshake`, and `serve_connection_with_cancel_and_handshake`. Capture is pay-for-what-you-use: the accept path builds a `HandshakeContext` only when such a hook is registered (or `accept_with_handshake` is called).

### Notes
- Strictly additive. No existing signature changes; `PeerRegistry::new()` and the existing `accept` / `serve_connection` paths are untouched.
- `HandshakeContext` is built for keying off an auth/token header, so two properties guard that use: `header(name)` is fail-closed — a value that is not valid (visible ASCII) text is dropped at capture rather than lossily replaced with U+FFFD, so a non-ASCII token reads as absent instead of a silently corrupted key — and its `Debug` is hand-written to redact header values and the query string, so a routine `tracing::debug!(?ctx)` cannot leak the credential the use case keys on.
- `is_websocket_upgrade` peeks only the connection's first readable chunk (bounded to 1 KiB). A false positive is caught by `accept`'s authoritative validation; a false negative (an `Upgrade` header split across TCP segments or beyond the peek window) routes to the HTTP handler with no recovery. The peek also blocks until the client sends a byte, so a production embedder should wrap it in a `tokio::time::timeout` to keep a silent client from pinning the task.
- The larger surfaces remain intentionally deferred per the crate's "hold higher-level surfaces until a second distinct consumer exists" discipline: a built-in HTTP route table, a generic `PeerRegistry<K>`, and an opaque `PeerHandle` tag.

## [3.1.0] - 2026-05-28

### Added
- `Message::into_wire_bytes(self) -> Vec<u8>` — consuming counterpart to `to_vec` for outbound paths where the message is shipped to a sink that takes an owned `Vec<u8>` (the built-in WebSocket writer and `UdpClient` send paths). Fast path is zero new allocations and one in-place body memcpy via `copy_within` when the body already has capacity for the `HEADER_SIZE + query.len()` prefix; opt in by constructing bodies via `Vec::with_capacity(body_len + HEADER_SIZE + query.len())` and feeding them to `MessageBuilder::body_bytes`. Slow path falls back to a fresh allocation matching `to_vec`'s cost while still releasing the original query and body buffers as soon as the wire bytes are produced. `Message::to_vec` is unchanged.
- Two `criterion` benches under `benches/`: `wire_serialization` measures `to_vec` against both `into_wire_bytes` paths across body sizes from 64 B to 4 MiB; `route_dispatch` measures `Router::get` and full `get + handle` cycles across plain HashMap, `Registry`-backed, and `RepeStruct` routes, each with and without an attached middleware. Pulled in as a `dev-dependencies` entry only.

### Changed
- Routing dispatch is now allocation-free per request on registry and struct routes. `RegisteredRegistry` and `RegisteredStruct<T, L>` implement `HandlerErased` directly: each registration builds one handler `Arc<dyn HandlerErased>` once and `Router::get` is now a prefix check (or HashMap probe) plus a single `Arc::clone`. Previously every registry hit allocated a fresh `String` for the JSON pointer and an `Arc::new(RegistryRequestHandler { ... })`; every struct hit allocated a `Vec<String>` from `json_pointer::parse`, a `String`, and an `Arc::new(StructRequestHandler { ... })` — all gone. The escape-free struct fast path also drops segment `&str`s into a 16-slot stack buffer with heap overflow, skipping the `Vec<String>` from `json_pointer::parse` entirely for the typical 1–4 segment pointer.
- Middleware wrapping hoisted from `Router::get` to registration time. Each route entry carries both a `raw` and a `dispatched` handler `Arc`, where `dispatched` is `raw` already wrapped in any active `MiddlewarePipeline`. `Router::get` returns `dispatched` directly, so plain routes pay nothing per dispatch for middleware they do not touch and middleware-equipped routes no longer pay an `Arc::new(MiddlewarePipeline)` per request. `register_middleware` rebuilds the `dispatched` slot across every existing route so semantics stay uniform; cost is `O(N·K)` for `N` middleware registrations after `K` routes, paid only at builder time.
- WebSocket writer (`writer_task` and the proxy forwarding path) and `UdpClient::send` switch from `Message::to_vec` to `Message::into_wire_bytes`, picking up the slow-path savings without callers needing to change anything. Behavior is byte-identical; the fast path activates if callers opt in with a pre-reserved body.

### Fixed
- The `repe` CLI now compiles under `cargo check --all-features`. v3.0.0 added `#[non_exhaustive]` to `BodyFormat`, `QueryFormat`, and `ErrorCode` but did not update the bin's response-decoding match; bin targets compile as a separate crate so the attribute applies across the boundary, and `cargo check --all-features` failed on the bin until the unknown-variant wildcard landed.

### Notes
- Existing `create_response` builders produce `Vec<u8>` bodies with `capacity == len`, so today's WebSocket-server outbound responses land on the `into_wire_bytes` slow path (same memcpy cost as `to_vec`, with the body buffer released sooner). The fast path is available for callers that opt in — most relevantly `repe::stream` chunk producers building their own response bodies, where the prefix reservation removes the per-chunk body copy entirely.
- Bench numbers under `cargo bench --quick`: `router_get/plain_with_middleware` matches `router_get/plain` (~33 ns), `router_get/registry|struct` is ~10 ns, and `wire_serialization/into_wire_bytes_fast` is roughly 1.5× the throughput of `to_vec` at 4 KiB and 7× at 64 KiB.

## [3.0.0] - 2026-05-27

### Added
- Off-reader handler cancellation on `CallContext`: `CallContext::is_cancelled()` (non-blocking) and `CallContext::cancelled()` (a `Future`) report/resolve when the calling peer disconnects or the server is shutting down. A long `_blocking` handler should poll `is_cancelled()` at loop boundaries and return early to free its `spawn_blocking` thread instead of running pointless work to completion. Both degrade to a never-cancelling no-op on peer-less transports (TCP servers, in-process dispatch), mirroring `CallContext::peer()` returning `None`. Backed by a per-connection `tokio_util` `CancellationToken` cancelled from the disconnect `Drop` guard; the backing type is not exposed. Complements — does not replace — the `on_peer_disconnect` → `TransferControl::cancel` path, which remains the only thing that wakes a producer parked in `wait_for_credit`.
- Graceful connection drain: `WebSocketServer::serve_with_graceful_drain(addr, path, shutdown, drain_timeout)` and `serve_listener_with_graceful_drain(listener, path, shutdown, drain_timeout)`. On shutdown they stop accepting, cancel in-flight off-reader handlers, await already-accepted connections (tracked in a `JoinSet`) until `drain_timeout`, then abort whatever remains. Aborting a connection tears down its writer task too, so the server-level `drain_timeout` supersedes the per-connection 5 s writer drain. The turnkey counterpart to `serve_with_shutdown`, which still returns immediately and detaches connections.
- `ShutdownToken` (re-exported at the crate root) and `SharedWebSocketServer::serve_connection_with_cancel(ws, &token)`, for a one-port co-hosting embedder that owns its own accept loop: create one token, serve each connection through it, and call `token.cancel()` on shutdown to wake every connection's in-flight off-reader handlers while the embedder drains its own task set. The backing `tokio_util` token is hidden so embedders are not coupled to that crate's version.
- `WebSocketServer::on_error(f)` and the `ConnectionError` type (re-exported at the crate root): a hook for transport-level events — handshake failures, connection I/O errors (in the built-in accept loops), caught off-reader handler panics, and off-reader saturation rejections — so an embedder can route them into its own `log` / `tracing` pipeline. Composes in registration order like `on_peer_connect` / `on_peer_disconnect`. With no hook registered the server keeps its historical `eprintln!` behavior (and stays silent on saturation, which never logged). `ConnectionError::error_code()` maps the panic / saturation categories to the `ErrorCode` that reached the client.
- `ErrorCode::ResourceExhausted` (8) and `ErrorCode::InternalError` (9), in the REPE-reserved `8..4095` range between `Timeout` and `ApplicationErrorBase`. Resolves the protocol gap noted in the 2.6.0 release notes: a client can now branch retry-on-busy (saturation) vs. surface-on-error (real failure) by code alone. Additive at the library level; interoperating on these codes with other REPE implementations requires they agree on the codes' meaning.
- `examples/websocket_graceful_server.rs`: a runnable end-to-end demo of a cancellation-polling off-reader handler, an `on_error` hook, and `serve_listener_with_graceful_drain`.

### Changed
- `ErrorCode`, `QueryFormat`, and `BodyFormat` are now `#[non_exhaustive]`, so downstream `match` expressions on these protocol enums must add a wildcard (`_`) arm. This is the one-time source-breaking change behind the major version bump; in exchange, future additions to these enums — new error codes in the reserved `8..4095` range, new query/body formats — ship as non-breaking minor releases.
- A caught off-reader handler panic now responds with `ErrorCode::InternalError` and an off-reader saturation rejection with `ErrorCode::ResourceExhausted` (both were `ErrorCode::ApplicationErrorBase`). A client that matched `ApplicationErrorBase` to detect these two cases must update to the new codes.
- The three WebSocket-server `eprintln!` sites (handshake error, connection error, off-reader handler panic) route through `on_error` when a hook is registered, falling back to the same stderr output otherwise.

### Notes
- `tokio-util` (with its `rt` feature, for `CancellationToken`) is a new dependency, pulled in only with the `websocket` feature.
- The cancellation signal wakes poll-loop (`is_cancelled()`) and async (`select!` on `cancelled()`) handlers; it does **not** wake a producer parked in `TransferControl::wait_for_credit` (a `Condvar`), which is still woken only by `TransferControl::cancel` — drive that from `on_peer_disconnect` as before. The two compose: cancellation winds a handler down promptly so the drain timeout is a backstop rather than the common wait.
- A misbehaving off-reader handler that never polls its cancellation signal still holds its `spawn_blocking` thread past `drain_timeout` (a blocking thread cannot be aborted); the drain's abort tears down the connection's reader/writer, not such a handler.
- A first-class `WebSocketServer` stream-source surface (owning begin/ack/cancel/resume internally) remains deferred until a second distinct windowed-transfer consumer exists. Its prerequisites — cancellation-on-disconnect and orderly drain — now land, so it can be designed against this consumer once a second appears.

## [2.6.0] - 2026-05-26

### Added
- Off-reader handler dispatch: `Router::with_json_blocking`, `with_json_ctx_blocking`, `with_typed_blocking`, and `with_typed_ctx_blocking`. On `WebSocketServer` a `_blocking` route runs on a `tokio::task::spawn_blocking` thread so the connection's reader stays free to decode further inbound frames (ACKs, cancels, resumes) while the handler runs or parks. This is the prerequisite for driving a `repe::stream` producer directly from a request handler: the producer parks in `wait_for_credit` waiting on ACKs that arrive as inbound frames the same reader must decode, so an inline handler would deadlock at the first full window. The TCP `Server` / `AsyncServer` ignore the tag and always run inline (they have no peer and no push primitive).
- `Execution` enum (re-exported at the crate root) and `HandlerErased::execution()`, returned by the trait to tell `WebSocketServer` where to dispatch. Defaults to `Execution::Inline`, so existing handlers and custom `HandlerErased` implementors are unaffected; the `_blocking` constructors return `Execution::OffReader`.
- `WebSocketServer::with_offreader_limit(n)` and `DEFAULT_OFFREADER_LIMIT` (16): per-connection cap on concurrently running off-reader handlers. When the cap is reached a further off-reader request gets an immediate error response (the client retries) rather than blocking the reader — blocking it would stall the very ACK/cancel frames in-flight transfers need to free a slot. Pass `0` to remove the cap.
- Graceful shutdown: `WebSocketServer::serve_with_shutdown(addr, path, shutdown)` and `serve_listener_with_shutdown(listener, path, shutdown)` stop accepting once the `shutdown` future resolves and return `Ok(())`. Already-accepted connections run on their own detached tasks and are not awaited; track them via a `PeerRegistry` if you need to drain them.
- One-port co-hosting: `WebSocketServer::into_shared()` returns a cheap, cloneable `SharedWebSocketServer` (re-exported at the crate root). Pair `WebSocketServer::accept(stream, path)` (the REPE handshake) with `SharedWebSocketServer::serve_connection(ws)` to serve connections the embedder accepts itself — e.g. share one TCP port between REPE WebSocket upgrades and the embedder's own HTTP routes. Connect/disconnect hooks and any attached `PeerRegistry` fire exactly as under `serve`.
- `Next::ctx()` and `Next::peer()`: let a cross-cutting middleware read the calling `CallContext` / `PeerHandle` that `WebSocketServer` threads through the pipeline, without the middleware being a context-aware leaf handler itself. Both return `None` on peer-less transports (TCP, direct in-process dispatch).

### Changed
- `WebSocketServer`'s reader now resolves each request (version/query validation + handler lookup) and then dispatches it inline or off-reader based on `HandlerErased::execution()`. Validation and error-response synthesis are computed once on the reader and are identical on both paths; only where the handler body runs differs. Request/response servers with no `_blocking` routes see no behavior change.
- `MiddlewarePipeline` forwards `execution()` to the wrapped handler, so registering middleware no longer downgrades a `_blocking` route back to inline.
- The `serve` / `serve_listener` / `serve_with_shutdown` / `serve_listener_with_shutdown` entry points are unified onto a single accept loop built from the public `into_shared` + `accept` + `serve_connection` primitives.

### Notes
- Off-reader dispatch is strictly opt-in; inline remains the default and keeps strict per-connection, request-at-a-time ordering. Off-reader handlers run concurrently with subsequent inline handlers and with each other, so their responses interleave on the wire — correlate responses to requests by the REPE message id (as clients already do), not by arrival order.
- A producer parked off-reader holds a process-wide `spawn_blocking` thread (tokio's default pool is 512) until its `wait_for_credit` deadline, an idle-watchdog cancel, or an `on_peer_disconnect`-driven `TransferControl::cancel`. The hold is bounded, not indefinite, but embedders expecting many concurrent streaming connections should size the runtime's `max_blocking_threads` accordingly. See `docs/websocket.md`.
- Both the saturation rejection and a caught handler panic surface as `ErrorCode::ApplicationErrorBase`. REPE defines no protocol-level "unavailable" / "internal" code today, so a client cannot tell these apart from an ordinary application error by code alone. Treat this as a known protocol gap.
- A first-class `WebSocketServer` stream-source surface (owning begin/ack/cancel/resume internally) is deferred until a second distinct windowed-transfer consumer exists, so the trait is designed against more than one shape. Until then, off-reader dispatch plus the `repe::stream` API cover the single-consumer case.

## [2.5.1] - 2026-05-20

### Changed
- Gate `PeerRegistry::id_counter` behind the `websocket` feature so default-feature builds don't warn that the method is dead code; it is only consumed by the feature-gated `websocket_server`. No API or behavior change for `websocket`-enabled builds.

## [2.5.0] - 2026-05-20

### Added
- `PeerRegistry`: cloneable live set of connected peers, with `broadcast_notify_json` / `broadcast_notify_beve` / `broadcast_notify_utf8` / `broadcast_notify_raw` helpers. Each broadcast encodes (or copies, for the pre-encoded variants) the body once on the caller's task and returns a `HashMap<PeerId, Result<(), PeerSendError>>` so callers can prune dead peers (`PeerSendError::Disconnected`) or surface backpressure (`PeerSendError::Full`). Owns its own `PeerId` allocator (`PeerRegistry::next_peer_id`), so two `WebSocketServer`s sharing one registry never mint colliding ids.
- `WebSocketServer::with_peer_registry(registry)`: attach a `PeerRegistry` so accepted peers are inserted on connect and removed on disconnect. The disconnect cleanup runs from a `Drop` guard so it fires on every exit path (clean close, transport error, handler panic).
- `WebSocketServer::on_peer_connect(f)` / `on_peer_disconnect(f)`: lower-level lifecycle hooks. Both compose: every registered closure fires in registration order, so `with_peer_registry` can coexist with logging or metrics callbacks. The connect hook runs synchronously before the reader/writer tasks start, so a notify queued from inside it is guaranteed to reach the wire before any response.
- `WebSocketServer::with_outbound_capacity(n)`: per-connection outbound channel capacity (default `DEFAULT_OUTBOUND_CAPACITY = 256`). A full channel returns `PeerSendError::Full` from `PeerHandle::send_notify`; the embedder picks the retry/prune policy.
- `Router::with_json_ctx` and `Router::with_typed_ctx`: register handlers that take a `&CallContext` alongside the body. Inside the handler, `ctx.peer()` is the calling `PeerHandle` (when the request came in over a transport that produces peers; `WebSocketServer` does), so handlers can push notifies back to the originator mid-request (progress updates, streaming chunks, server-initiated state mirroring).
- `HandlerErased::handle_with_ctx(&self, req, ctx)`: new trait method threading the `CallContext` to the leaf handler. Defaults to ignoring the context and calling `handle`, so existing implementors (embedder custom handlers) compile unchanged.
- `TypedHandlerFnCtx` in `repe::server`: trait shape mirroring `TypedHandlerFn` for `Fn(&CallContext, T) -> Result<R, ...>` closures.
- `route_request_with_ctx` (crate-internal): peer-threaded dispatch path used by `WebSocketServer`. TCP-backed servers continue to use the existing context-free `route_request`.

### Changed
- `WebSocketServer::handle_connection` is now a reader/writer task pair coordinated by a bounded `tokio::sync::mpsc` channel. The writer task is the sole point that touches the outbound `SplitSink`, so any code path that holds a `PeerHandle` can push notifies onto the wire alongside in-flight responses. Existing request-response servers see no behavior change.
- `WebSocketServer` now dispatches each request through `HandlerErased::handle_with_ctx` with a `CallContext` carrying the calling peer. Handlers registered via `with_json` / `with_typed` are unaffected (they inherit the default `handle_with_ctx` that drops the context). TCP servers continue to dispatch via the existing context-free path.

### Notes
- The push path is strictly opt-in. `WebSocketServer::new(router).serve(...)` callers compile and run unchanged. No protocol changes: notify frames are the same shape they have always been.
- Broadcast cost: one encode plus one `Vec<u8>` clone per peer. Sharing a single `Arc<[u8]>` across peers for very large broadcast bodies is left as future work.

## [2.4.0] - 2026-04-27

### Added
- `cli` feature and `repe` binary: command-line client for REPE servers. Auto-detects transport from `--url` (`ws://` / `wss://` use the WebSocket transport, anything else uses TCP, with a default port of 5099). Supports `get` / `set` / `call` / `notify` subcommands, plus an inferred mode (`repe /path` reads, `repe /path '<json>'` writes). Install with `cargo install repe --features cli`.
- CLI body sources: pass `-` as the positional body to read from stdin, or `--body-file PATH` to read from a file. The two sources are mutually exclusive with each other and with a literal positional body.
- CLI response decoding handles all body formats: JSON and BEVE responses are pretty-printed (or compacted with `--raw`), UTF-8 bodies are surfaced as JSON strings (or printed verbatim under `--raw`, so `repe --raw get /motd` behaves like a plain text fetch), and unparseable raw-binary responses are surfaced as a clear RPC error rather than an opaque decode failure.
- `REPE_URL` environment variable as a default for `--url`, so repeated invocations against the same server can drop the flag. An explicit `--url` always overrides the env var.
- `repe completions <shell>` subcommand emits a clap-derived completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`. Runs locally with no server or runtime.
- `tests/cli.rs` end-to-end coverage of the binary against an in-process registry-backed `AsyncServer` and `WebSocketServer`, exercising every subcommand, raw output, stdin/`--body-file` body sources, the body-source conflict rule, both error-exit paths, `--timeout` enforcement, BEVE response decoding, `REPE_URL` precedence, and the completions subcommand.

### Fixed
- Bare bracketed IPv6 literals in `--url` (`[::1]`) now have the default port `:5099` appended, matching the IPv4 hostname behavior. Previously they were left untouched and produced invalid socket addresses.
- `--` is now an explicit opt-out from inferred mode: `repe -- /foo` no longer rewrites to `repe -- get /foo` (which clap would then misparse), it leaves argv untouched.
- `--timeout` is now honored by `notify`. The flag was previously declared global but had no plumbing through to `Transport::notify`, so `repe --timeout 1 notify /x '{}'` silently ignored the bound.
- `--timeout` rejects negative and non-finite values (NaN, `+/-inf`) with a clear usage error instead of silently clamping to zero (which then fired immediately).
- `--body-file` is rejected for `get` before the connect attempt, so misconfigured invocations against an unreachable server surface the usage error instead of the connect failure.

### Notes
- CLI bodies are validated as syntactically-valid JSON client-side before the request is sent. Semantic validation (does this value match the schema for `/foo`?) remains the server's responsibility.

## [2.2.0] - 2026-04-25

### Added
- `repe::stream` module: backpressure-controlled streaming over REPE notifies, for protocols that push large payloads (multi-GB blobs, log streams, paginated query results) from the server to a peer and need flow control beyond what the bare notify primitive provides.
  - `TransferControl`: per-transfer state machine. ACK-driven window credit (`wait_for_credit` / `record_sent` / `record_ack`), sticky cancel signal, replay ring, peer slot, idle timestamps.
  - `TransferRegistry<K>`: typed table of in-flight transfers; the embedder picks the key type (typically a transfer-id newtype). Inbound ACK / cancel / resume handlers look the control up by key.
  - Replay ring + reconnect: `push_replay`, `replay_chunks_from`, `request_resume`, `wait_for_reconnect`, `take_pending_resume`. The producer parks on `wait_for_reconnect` after a `PeerSendError::Disconnected`; an inbound resume handler calls `request_resume(new_peer, file_index, last_received_offset)` to swap in the new peer and unpark, after which the producer replays the ring tail.
  - `spawn_watchdog<K>(registry, idle_timeout)`: background thread that scans the registry and force-cancels transfers whose last chunk and last ACK are both older than `idle_timeout`.
  - `RingChunk` carries `body_bytes: Arc<Vec<u8>>` (the exact wire body); replay is a straight resend, not a re-encode.
- The wire shape of the protocol (`transfer_begin`, `file_chunk`, ACK / cancel / resume bodies) is up to the embedder; this module deals only in offsets, ACKs, and opaque body bytes.

### Notes
- The watchdog thread holds a `Weak<TransferRegistry<K>>`, not an `Arc`. Dropping the embedder's last strong reference terminates the thread on its next tick (clamped to `[1 s, 5 s]`). For a process-wide singleton this just means the thread lives for the process; embedders that build short-lived registries (per-test, per-tenant) get clean teardown for free.
- Defaults (`DEFAULT_WINDOW_BYTES`, `DEFAULT_BACKPRESSURE_TIMEOUT`, `DEFAULT_IDLE_TIMEOUT`, `DEFAULT_REPLAY_RING_BYTES`, `DEFAULT_RECONNECT_TIMEOUT`) are tuned for LAN-class links pushing multi-GB files. Lower the window on slow links; raise the reconnect timeout for clients with long roaming windows.

## [2.1.0] - 2026-04-25

### Added
- `WebSocketClient::subscribe_notifies()` returns `Result<UnboundedReceiver<Message>, AlreadySubscribed>` and yields inbound `Message`s whose `notify` header flag is set. The receiver-side body is decoded by the application (e.g. via `Message::json_body`, `beve::from_slice`, or `MessageView`).
- `WebSocketClient::unsubscribe_notifies()` clears the active subscription so a subsequent `subscribe_notifies` call can install a new one.
- `AlreadySubscribed` error type, returned when `subscribe_notifies` is called while a live subscription already exists. Stale slots whose prior receiver was dropped do not block resubscription; the new sender installs silently.

### Changed
- `WebSocketClient`'s response loop now routes inbound messages by the notify flag *before* consulting the request/response correlation map. Server-pushed notifies that happen to share an id with an in-flight request will no longer be dispatched to the request waiter.

### Notes
- `subscribe_notifies` returns an unbounded channel by design. A bounded variant is not offered: drop-on-full corrupts chunk streams and block-on-full stalls the shared request/response path. Consumers must drain the receiver promptly; high-rate notify protocols should layer their own backpressure on top.
- `WebSocketClient` is `Clone`, and the subscription slot is shared across clones. The loud-replace contract above is what keeps two holders of the same client from silently stealing each other's subscription; if one holder needs to take over from another, it must call `unsubscribe_notifies` first.

## [2.0.0] - 2026-04-25

### Breaking changes
- `RegistryCallable::call` now takes `&CallContext` in addition to `Option<Value>`. Plain `Fn(Option<Value>) -> Result<...>` closures keep compiling unchanged via the existing blanket impl, but anyone implementing `RegistryCallable` directly via a struct must update their `fn call` signature.

### Added
- Added streaming and zero-copy `Message` I/O for large bodies:
  - `Message::write_to<W: Write>` and `Message::serialized_len` emit/size a message without allocating an intermediate frame `Vec<u8>`.
  - `write_message_streaming(w, header, query, body_len, body_writer)` lets the body be produced by a closure (pairs with `beve::to_writer_streaming` for direct BEVE-into-writer encoding of multi-MiB bodies).
  - `MessageView<'a>` borrows the query and body slices out of a caller-supplied buffer instead of copying like `Message::from_slice`. Useful with `serde_bytes::Bytes<'a>` so a chunk payload stays borrowed end-to-end.
- Peer-aware handler routing types so handlers can push notify messages back to the calling peer:
  - `PeerSink` trait that embedders implement against their per-connection outbound mechanism.
  - `PeerHandle` (cloneable wrapper over `Arc<dyn PeerSink>` plus a `PeerId`), `NotifyBody` (variant tagged with the wire `BodyFormat`), and `PeerSendError`.
  - `CallContext<'a>` carries the optional `PeerHandle` and the dispatched method through to handlers.
  - `WithContext` adapter for registering closures that want the `&CallContext`.
  - `Registry::dispatch_with_ctx` mirrors `Registry::dispatch` but threads the `CallContext` to the registered callable. `dispatch` becomes a thin wrapper that supplies a `CallContext::detached` context.
  - Built-in TCP/WebSocket servers do not yet construct `PeerHandle`s themselves; embedders that need peer routing wire their own `PeerSink` and call `dispatch_with_ctx`.

## [1.1.0] - 2026-03-12
- Added WebSocket transport support:
  - `WebSocketClient` for native async WebSocket RPC
  - `WebSocketServer` for native WebSocket serving
  - `WasmClient` for browser-based WebSocket RPC on `wasm32-unknown-unknown`
- Added `proxy_connection` support for forwarding full REPE `Message` values to an upstream `AsyncClient`.
- Added `Message::from_slice_exact` and enforced exact bounded-message length validation for WebSocket binary frames.
- Refactored server request validation/routing into shared helpers so TCP sync, TCP async, and WebSocket servers use the same request handling path.
- Refactored crate exports and target-specific dependencies so shared protocol types compile on both native and `wasm32`, while native TCP transports remain gated off the wasm target.
- Gated native integration tests to `not(target_arch = "wasm32")` so wasm-target builds do not try to compile native TCP/fleet test binaries.

## [1.0.0] - 2026-03-05
- Added multi-node fleet APIs:
  - `Fleet` for synchronous TCP request/response fanout
  - `AsyncFleet` for asynchronous tokio-based TCP fanout
  - `UniUdpFleet` for unidirectional UDP fanout
- Added shared fleet types and configuration:
  - `NodeConfig`, `FleetOptions`, `RetryPolicy`
  - `RemoteResult`, `HealthStatus`
  - connect/disconnect/reconnect summary types
- Added fleet operations:
  - connection lifecycle (`connect_all`, `disconnect_all`, `reconnect_disconnected`)
  - single-node calls (`call_json`, `call_message`)
  - tag-filtered broadcast (`broadcast_json`) and reduction (`map_reduce_json`)
  - per-node health checks (`health_check`)
- Updated TCP fleet retry policy to retry only transport/I/O failures and stop retrying on application-level server errors.
- Added UDP foundations:
  - `UniUdpClient` with notify/request send APIs and per-message IDs, backed by the `uniudp` crate
  - UDP node config with redundancy/chunk/FEC fields
  - UniUDP default RS profile now uses `fec_group_size=4` with `parity_shards=2`
- Made UniUDP support opt-in behind the `fleet-udp` Cargo feature so TCP-only builds avoid UniUDP dependencies.
- Added integration tests for:
  - sync fleet behavior (`tests/fleet_tests.rs`)
  - async fleet behavior (`tests/async_fleet_tests.rs`)
  - UDP fleet behavior (`tests/uniudp_fleet_tests.rs`)
- Added fleet documentation (`docs/fleet.md`) and README examples.

## [0.4.2] - 2026-02-22
- Added multiplexed request handling to `Client` and `AsyncClient` so multiple in-flight calls can share a single connection and still match responses by request ID.
- Added per-request timeout helpers on both clients:
  - `call_json_with_timeout`
  - `call_typed_json_with_timeout`
  - `call_typed_beve_with_timeout`
- Added JSON batch helpers on both clients:
  - `batch_json`
  - `batch_json_with_timeout`
- Hardened unknown-response-ID handling:
  - unknown response IDs are now logged and dropped by default
  - late responses for timed-out requests are also dropped without tearing down the connection
- Made `AsyncClient` request tracking cancellation-safe so dropped call futures do not leak entries in the pending-request map.
- Preserved structured fatal response-loop errors when failing pending requests instead of flattening everything to `Io(ConnectionAborted)`.
- Bounded sync `Client::batch_json` worker threads to avoid unbounded OS thread creation on large batches.

## [0.4.0] - 2025-10-24
- Router middleware hooks (`with_middleware` / `register_middleware`) let servers centralize auth, logging, or validation without manually wrapping each handler.
- Router shared-struct registration now accepts any `Lockable` lock, including `tokio::sync`
  mutexes/RwLocks out of the box and `parking_lot` locks when the optional feature is enabled.
- Bumped the edition to Rust 2024 and raised the MSRV to 1.85.

## [0.2.0] - 2025-09-18
- Added full BEVE body support (builder helpers, response serialization, and message decoding) backed by the official `beve` crate.
- Server routers and typed handlers now accept BEVE payloads, mirroring existing JSON ergonomics.
- Documented BEVE usage and updated the spec reference URL; expanded tests to cover complex BEVE round-trips.

## [0.1.3] - 2025-09-17
- Added zero-copy query routing so servers reuse borrowed UTF-8 query slices, cutting per-request allocations and tightening error handling for invalid query encodings.

## [0.1.2] - 2025-09-17
- Stream sync and async REPE message I/O directly into final buffers to avoid extra allocations and copies.
- Reuse persistent buffered readers/writers in the TCP client to eliminate per-request socket clones.
