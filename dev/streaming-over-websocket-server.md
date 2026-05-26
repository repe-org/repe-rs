# Driving `repe::stream` from `WebSocketServer`: off-reader dispatch and related gaps

## Implementation status & handoff (read first)

This started as a design proposal. **Feature 1 is implemented**; the rest is a work order for an implementing agent. Written against repe 2.5.1.

**Done.** Feature 1 (off-reader handler dispatch) landed in commit `c19f86f` on branch `streaming-over-websocket` (draft PR #13, base `main`). See [Feature 1](#feature-1-required-off-reader-handler-dispatch) for exactly what exists now.

**Remaining work for this PR**, in recommended order:

1. [Feature 5](#feature-5-minor-expose-callcontext-to-middleware) — `Next::ctx()` accessor. Trivial, additive, no dependencies.
2. [Feature 3](#feature-3-graceful-shutdown) — `serve_with_shutdown`.
3. [Feature 4](#feature-4-serve-an-already-accepted-connection-one-port-co-hosting) — `into_shared` / `accept` / `serve_connection` for one-port co-hosting.
4. [Feature 1 follow-ups](#feature-1-follow-ups) — an end-to-end `repe::stream`-over-off-reader integration test, and a `docs/websocket.md` section for the `_blocking` constructors.

**Out of scope.** [Feature 2](#feature-2-optional-future-first-class-stream-source-on-websocketserver) (`StreamSource`) stays **deferred** per [decision 5](#resolved-design-decisions); do not implement it.

**Constraints (binding).**

- Everything is **additive and opt-in**: existing `serve` / `with_json` / `with_typed` callers must compile and behave unchanged. **No wire-format or frame-shape changes.**
- The [Resolved design decisions](#resolved-design-decisions) are binding; honor them.
- **Do not add new `ErrorCode` variants.** REPE's codes are a shared cross-language spec; the missing "unavailable"/"internal" code is a known gap ([decision 3](#resolved-design-decisions)) to raise upstream, not patch here. Keep the interim `ErrorCode::ApplicationErrorBase` that Feature 1 already uses.
- Match the surrounding code's style and comment density; put rustdoc on every new public item.

**Workflow.** Work on `streaming-over-websocket`; commit each feature separately; keep PR #13 a draft until the list above is complete, then mark it ready.

**Verify before each commit (all must pass):**

```
cargo test --features websocket            # lib + integration + doctests
cargo test                                 # default features (TCP path)
cargo clippy --features websocket --all-targets
cargo fmt --check
```

**Note on line numbers.** References below predate the Feature 1 implementation and have shifted; locate code by the named symbols (functions, structs, fields), not by line. Current anchors are given where they matter.

This note is about two subsystems that shipped independently and do not currently compose: the push-capable `WebSocketServer` (reader/writer task pair, `PeerHandle`, `PeerRegistry`) and the `repe::stream` backpressure control plane (`TransferControl`, `TransferRegistry`, replay ring, watchdog). An embedder that tries to drive a windowed transfer from inside a `WebSocketServer` request handler deadlocks. Fixing that cleanly needs one new primitive (off-reader handler dispatch); a few smaller server gaps surfaced alongside it and are folded in here.

## Background: the two subsystems and their concurrency assumptions

**`WebSocketServer`** runs one reader task and one writer task per connection, coordinated by a bounded outbound channel. The reader decodes each inbound frame and dispatches the matching handler; the writer is the sole owner of the outbound sink, so responses and `PeerHandle`-pushed notifies share one ordered path to the wire. The reader dispatches **inline and synchronously**: handlers are ordinary synchronous functions, so the `reader_task` poll runs the handler to completion on the connection's task before it reads the next frame (`src/websocket_server.rs:321`, inside the loop that awaits `ws_reader.next().await`). A handler that parks therefore stalls more than the next frame: it pins the runtime worker the reader is polled on, and on a `current_thread` runtime it wedges the entire server.

**`repe::stream`** adds flow control on top of notify frames. Its documented model (see `docs/streaming.md` and the `src/stream.rs` module doc) is a **blocking producer**: a thread calls `wait_for_credit` (synchronous, parks on a `Condvar`), sends a chunk notify, calls `record_sent`, and repeats. The window is released by a **separate** caller: "inbound ACK handlers call `record_ack`". So `repe::stream` assumes two things hold at once:

1. the producer runs on a thread that is free to block, and
2. some other task is free to receive inbound ACK frames and call `record_ack` while the producer is parked.

Those two assumptions are exactly what `WebSocketServer`'s inline reader cannot satisfy when the producer is started from a handler.

## The problem: inline dispatch deadlocks a handler-driven transfer

The natural way to expose a download is a request handler that starts the transfer:

```rust
let router = Router::new()
    .with_json_ctx("/download/begin", move |ctx, _body| {
        let peer = ctx.peer().expect("websocket peer").clone();
        let control = TransferControl::new(DEFAULT_WINDOW_BYTES);
        control.set_peer(peer);
        registry.register(TransferId(1), Arc::clone(&control));

        // Produce chunks: wait_for_credit -> peer.send_notify -> record_sent.
        // The window is released by record_ack, which is called from the
        // "/download/ack" handler. But that handler runs on the SAME reader
        // task we are occupying right now, so it cannot run until we return.
        // wait_for_credit blocks forever at the first full window. Deadlock.
        Ok(json!({ "started": true }))
    })
    .with_json("/download/ack", move |body| {
        let ack: Ack = serde_json::from_value(body)?;
        if let Some(c) = registry.get(TransferId(1)) { c.record_ack(ack.file_index, ack.offset); }
        Ok(json!({ "ok": true }))
    });
```

The reader is parked inside `/download/begin` for the life of the transfer, so inbound `/download/ack` frames are never decoded, `record_ack` is never called, and the credit window never reopens. The transfer stalls at the first full window (default 64 MiB). Because the handler is synchronous, the park also holds the runtime worker the reader was polled on for the duration; on a single-threaded runtime nothing else on the server makes progress either.

The only way to use the two together today is to **not** drive the producer from the handler: spawn a producer thread the server does not own, return from the handler immediately, and deliver the eventual result as a later notify rather than as the request's response. That works, but it pushes the producer lifecycle back onto the embedder, changes request/response semantics into fire-and-forget-plus-notify, and bypasses the server's dispatch entirely, which is the wiring `WebSocketServer` was meant to absorb.

The root cause is not where any code lives. `repe::stream` is already the right module and draws the right boundary (it deals only in offsets, ACKs, and opaque bytes). The root cause is that `WebSocketServer`'s inline reader and `repe::stream`'s "free reader feeding `record_ack`" requirement are contradictory. The fix is a way to run a handler off the reader task.

## Feature 1 (required): off-reader handler dispatch

> **Status: implemented** (commit `c19f86f` on `streaming-over-websocket`). The design below is realized as:
>
> - `Execution` enum + `HandlerErased::execution()` (default `Inline`) in `src/server.rs`, re-exported as `repe::Execution`.
> - `Router::with_json_blocking` / `with_json_ctx_blocking` / `with_typed_blocking` / `with_typed_ctx_blocking`, each wrapping the handler in `OffReaderHandler` (`src/server.rs`).
> - `MiddlewarePipeline::execution()` forwards to the wrapped handler.
> - `route_request_with_ctx` factored into `resolve` + `dispatch` (`src/server_request.rs`).
> - `reader_task` (`src/websocket_server.rs`) resolves, checks `execution()`, then runs `dispatch` inline or via `spawn_off_reader` (same file): `spawn_blocking` + `catch_unwind`, response via `blocking_send`.
> - `WebSocketServer::with_offreader_limit(n)` + `DEFAULT_OFFREADER_LIMIT` (16); per-connection `Semaphore`; saturation and caught panics return `ErrorCode::ApplicationErrorBase`.
> - Tests in the `server.rs` and `websocket_server.rs` `tests` modules (execution defaults, middleware forwarding, parking, panic, saturation).
>
> The prose below is the original design rationale, kept as the established patterns later features must follow. Two pieces remain — see [Feature 1 follow-ups](#feature-1-follow-ups).

Let a handler run on a thread separate from the connection reader, so the reader keeps decoding inbound frames (ACKs, cancels, resumes) while the handler runs. The handler's response, if any, flows back through the existing writer task on completion.

### Proposed API

Opt-in registration variants that tag a path as off-reader, mirroring the existing families:

```rust
Router::new()
    .with_json("/status", ..)               // inline, as today
    .with_json_blocking("/download/begin", ..)      // runs off the reader
    .with_json_ctx_blocking("/export", ..)          // ctx + off the reader
    .with_typed_blocking("/report", ..)
    .with_typed_ctx_blocking("/stream", ..)
```

The `_blocking` suffix is the chosen name (see [decision 1](#resolved-design-decisions)): it follows the Rust convention where `blocking` describes the callee — "runs on the blocking pool and may park" — which is exactly the contract here.

### Behavior

When the WS reader dispatches a path registered off-reader, instead of awaiting it inline it:

1. clones the decoded request, the connection's `outbound_tx`, the calling `PeerHandle`, and the handler `Arc`,
2. calls `tokio::task::spawn_blocking(move || { .. })` and **does not** await the join handle,
3. immediately continues the read loop.

Resolution happens once, on the reader. It has to: reading `execution()` requires the resolved handler (step 1 already clones that `Arc`), and the version/query-format/method-not-found checks that precede resolution run there too. So the blocking task never re-resolves — it owns the resolved `Arc<dyn HandlerErased>` and runs only the **dispatch** step: reconstruct a `CallContext` from the owned peer and path, decode the body, call the handler, map the `Result` to a response. Concretely, factor `route_request_with_ctx` into `resolve` (version check → query-format check → `router.get`, returning either an early response or the resolved handler) and `dispatch` (the handler call plus error mapping); the reader runs `resolve` for every request, then runs `dispatch` inline or hands the resolved `Arc` to the blocking task. Because both paths share the one `dispatch`, the `Result` outcomes — handler-error→error-response, notify→no-response, and the early version/format/not-found responses from `resolve` — are identical by construction. For a request that yields a response the blocking task sends it with `outbound_tx.blocking_send(resp)`. `spawn_blocking` (not `tokio::spawn`) is correct here: handlers are synchronous and a `repe::stream` producer blocks on a `Condvar`, so it must not occupy a runtime worker.

**Panics are the one deliberate exception to "identical."** Inline, a handler panic unwinds the connection task, drops the `DisconnectGuard` (`src/websocket_server.rs:257`) so disconnect hooks fire, and tears the connection down. Off the reader, the panic is caught by `spawn_blocking` and parked in the `JoinHandle`; since the reader never awaits it, it is swallowed silently and the client hangs until its own timeout. (The disconnect path *not* firing is correct here — the connection is healthy — so the only real defect is the lost response.) Replicating the inline teardown would actually be wrong: an off-reader handler shares its connection with other concurrent off-reader transfers, so killing the connection over one handler's panic would abort all of them. The dispatch step therefore wraps the call in `catch_unwind` (via `AssertUnwindSafe`, since the handler `Arc` and friends are not `UnwindSafe`) and maps a panic to an error response carrying the request id, so `/download/begin` still "returns" — as a failure — and the connection survives. The code that response should carry is the same protocol gap as saturation (REPE has no "internal error" code; see [decision 3](#resolved-design-decisions)).

### Dispatch-site mechanics

The reader needs to know a path is off-reader before invoking it. The least invasive shape is a default method on the handler trait:

```rust
pub enum Execution { Inline, OffReader }

trait HandlerErased {
    fn handle(&self, req: &Message) -> Result<Message, RepeError>;
    fn handle_with_ctx(&self, req: &Message, ctx: &CallContext) -> Result<Message, RepeError> { .. }
    fn execution(&self) -> Execution { Execution::Inline }   // new, default Inline
}
```

The `with_*_blocking` constructors wrap the handler in a type whose `execution()` returns `OffReader`. The WS reader checks `execution()` and branches; the TCP servers ignore it (stay inline). Existing custom `HandlerErased` implementors inherit the default and are unaffected.

One correctness detail the trait-method shape must not miss: `Router::get` wraps the resolved handler in a `MiddlewarePipeline` whenever any middleware is registered (`wrap_handler`, `src/server.rs:733`), and the reader calls `execution()` on whatever `get` returns. `MiddlewarePipeline` must therefore forward `execution()` to its inner handler; without that one-line delegation, registering a single middleware silently downgrades every off-reader path back to inline and reintroduces the deadlock. Registry- and struct-backed handlers are built fresh per `get` and have no `_blocking` constructor, so they stay `Inline` — acceptable, since those are request/response surfaces, not producers. (The alternative — storing the execution mode as route metadata in the `Router` map instead of on the handler — sidesteps the forwarding trap but changes `Router::get`'s return type and the registry/struct resolution paths; that is more invasive than the current need warrants, so the trait method stands with the forwarding requirement made explicit.)

### Ordering semantics (must be documented)

Inline handlers keep strict per-connection, request-at-a-time order. Off-reader handlers run concurrently with subsequent inline handlers and with each other; their responses are interleaved on the wire and correlate by message id, which REPE already supports. Embedders must not assume per-connection serialization for off-reader paths. This is the same concurrency an embedder gets today if it hand-rolls a worker thread, made first-class and opt-in.

Removing that serialization also removes an implicit safety net, and one consequence is non-obvious. Inline, a second `/download/begin` for the same transfer id cannot start until the first returns; off-reader, two `begin` frames spawn two producers against one `TransferControl`, violating its documented single-producer assumption (`wait_for_credit`'s note, `src/stream.rs:478`) and racing the unconditional `registry.register` the sketch shows (lines 30, 41) — the second `begin` overwrites the registry entry while both producers run. The concurrency cap (decision 3) bounds the *count* of off-reader tasks, not duplicate registration, so the embedder's `begin` handler must reject (or de-duplicate) a transfer id that is already registered. When Feature 2 owns `begin`, repe should do this itself; a small `TransferRegistry` affordance would make the check race-free — e.g. a `try_register` that fails if the key is present, or having `register` return any control it displaced instead of silently overwriting.

### Concurrency limit

Off-reader dispatch is capped per connection (see [decision 3](#resolved-design-decisions)). `spawn_blocking` draws from tokio's process-global blocking pool (512 threads by default), and a producer parked in `wait_for_credit` holds its thread for the whole transfer, so an unbounded off-reader path lets a few connections starve every other blocking task in the process. A per-connection semaphore (`with_offreader_limit(n)`, finite default) bounds it; the permit is acquired before the spawn and held (moved into the task) for the task's whole life, so the count reflects actually-running handlers and a finished or aborted handler frees its slot on exit. On saturation the reader must **not** block waiting for a slot — that would stop it draining the ACK/cancel frames the in-flight transfers need to finish and free a slot, recreating the very deadlock this feature removes — so it `try`-acquires and, on failure, synthesizes an error response (an application error at/above `ErrorCode::ApplicationErrorBase`, since REPE defines no protocol-level "busy" code; see [decision 3](#resolved-design-decisions) on that gap) and keeps reading.

This composes with the existing idle watchdog to reclaim *wedged* slots, not just finished ones. `spawn_watchdog` runs on a dedicated `std::thread` (`src/stream.rs:690`), not a blocking task, so it is immune to the pool exhaustion it guards against; on idle timeout it fires `cancel`, `wait_for_credit` returns `Cancelled`, the producer exits, and both its blocking-pool thread and its semaphore permit are released. So the cap bounds new concurrency while the watchdog reclaims stuck transfers — together they bound exposure even against a peer that opens transfers and then goes silent.

### Scope

WebSocket server only (see [decision 2](#resolved-design-decisions)): that is where notify-driven streaming lives. The TCP `Server`/`AsyncServer` keep inline dispatch (request/response, no peer, no producer) and never consult `Execution`.

### Why this rather than the fire-and-forget workaround

Off-reader dispatch keeps the obvious request/response shape (`/download/begin` returns its result; the client does not have to special-case a result-bearing notify), needs no embedder-side producer plumbing, and gives exactly one correct way to combine a handler with `repe::stream`. The workaround leaves a footgun in place (drive the producer inline and it deadlocks) and re-imposes the per-embedder wiring the server exists to remove.

## Feature 1 follow-ups

Two pieces deferred from the initial Feature 1 commit; both are part of finishing this PR:

1. **End-to-end streaming integration test.** Feature 1's tests prove the reader stays free (a parked off-reader handler does not block other requests) but stop short of a real `repe::stream` transfer. Add a test that drives a `TransferControl` producer from a `with_json_ctx_blocking` `/begin` handler (the `wait_for_credit` → `peer.send_notify` → `record_sent` loop), routes inbound ACKs through an inline `/ack` handler that calls `record_ack`, and asserts a multi-window transfer completes over the WebSocket transport with the client draining notifies. This is the scenario the whole proposal exists to enable; put it in the `websocket_server.rs` `tests` module.
2. **`docs/websocket.md` section.** Document the `_blocking` constructors and `with_offreader_limit`: when to reach for off-reader dispatch (handlers that block or park, e.g. stream producers), the ordering caveat (off-reader responses interleave and correlate by message id), the per-connection cap and its saturation error, and the duplicate-`begin` caveat from [Ordering semantics](#ordering-semantics-must-be-documented). Rustdoc on the constructors already exists; this is the prose guide.

## Feature 2 (optional, future): first-class stream source on `WebSocketServer`

> **Out of scope for this PR** — deferred per [decision 5](#resolved-design-decisions). Do not implement unless explicitly re-scoped; building it speculatively contradicts a recorded decision. Recorded below for when a second windowed-transfer consumer triggers it.

Feature 1 makes a handler-driven transfer *possible* but still leaves the embedder to spawn the producer, register the `TransferControl`, and route inbound ack/cancel/resume into it through three separate handlers. It is also still possible to wire it wrong by producing inline. A higher-level surface could own all of that:

```rust
WebSocketServer::new(router)
    .with_stream_endpoint("/download", source /* : impl StreamSource */)
    .serve(addr, "/repe").await?;
```

where `StreamSource` yields opaque `(offset, last, body)` chunks on demand. repe would, on a `begin` frame, install a `TransferControl`, spawn the producer off the reader, and internally route `"/download/ack" | "/download/cancel" | "/download/resume"` frames into that control without embedder handlers. The embedder supplies only the byte source, the path prefix, and the chunk body method names.

Boundary: this stays domain-agnostic. repe owns the data plane (windowed byte streaming, ACK routing, replay, reconnect); the embedder owns the control/handshake plane (what `begin` validates, what the chunk body struct is). No file, manifest, hash, or compression concept enters repe.

Recommendation: defer until a second consumer needs windowed transfer over the built-in server (see [decision 5](#resolved-design-decisions)). Feature 1 plus the existing `repe::stream` API is sufficient for a single consumer, and building this speculatively risks over-fitting the trait to one transfer shape.

## Feature 3: graceful shutdown

`serve` / `serve_listener` loop forever (the accept `loop` in `serve_listener`); the only ways to stop are to drop the listener or abort the task. Add a shutdown-aware entry point:

```rust
pub async fn serve_with_shutdown<A: ToSocketAddrs>(
    self, addr: A, path: &str, shutdown: impl Future<Output = ()>,
) -> std::io::Result<()>;
```

It selects between `listener.accept()` and `shutdown`; on shutdown it stops accepting and returns. Priority: minor; useful for embedders that run the server in-process and need a clean stop without tearing down the runtime.

**Tasks.**

- Add `serve_with_shutdown` beside `serve_listener`; `tokio::select!` between `listener.accept()` and the `shutdown` future. On `shutdown`, break the accept loop and return `Ok(())`.
- First cut: stop accepting and return; let already-spawned connection tasks finish on their own (they are detached `tokio::spawn`s today). State this explicitly in the rustdoc.
- Refactor `serve_listener` to delegate to `serve_with_shutdown(.., std::future::pending())` so there is one accept loop, not two.

**Acceptance.** After `shutdown` resolves, the future returns `Ok(())` and no new connections are accepted; existing `serve` / `serve_listener` behavior is unchanged.

**Test.** Bind a listener, spawn `serve_with_shutdown` with a `oneshot`/`Notify` shutdown; confirm a client connects and round-trips before shutdown; fire shutdown; confirm the serve future returns and a subsequent connect no longer succeeds.

## Feature 4: serve an already-accepted connection (one-port co-hosting)

`serve_listener` owns the accept loop, and the per-connection pieces are private (`accept_repe_websocket` at `src/websocket_server.rs:240`, `handle_connection_with_config` at `:254`). An embedder that also serves plain HTTP routes therefore cannot share one TCP port with the REPE endpoint. Expose the per-connection path:

```rust
impl WebSocketServer {
    // Consume the builder once into a cheap, cloneable shared handle.
    pub fn into_shared(self) -> SharedWebSocketServer;

    // Perform the REPE WS handshake on an accepted stream (validates `path`).
    // Associated fn: needs only the path, not the router/config.
    pub async fn accept(stream: TcpStream, path: &str)
        -> Result<WebSocketStream<TcpStream>, RepeError>;
}

impl SharedWebSocketServer {   // Clone == Arc clone; Send + Sync + 'static
    // Run one connection's reader/writer loop using this server's router/hooks/capacity.
    pub async fn serve_connection(&self, ws: WebSocketStream<TcpStream>)
        -> Result<(), RepeError>;
}
```

An embedder can then peek the upgrade header on each accepted stream, route WebSocket upgrades to `accept` + `serve_connection`, and send everything else to its own HTTP handler, all on one listener.

Design note: `serve` currently consumes `self` and builds `Arc<ConnectionConfig>` once, so serving many connections by reference wants that config build factored out behind a shared handle — `into_shared(self) -> SharedWebSocketServer` with `serve_connection(&self, ws)` per connection (see [decision 4](#resolved-design-decisions)). The `serve_one` test helper (`src/websocket_server.rs:720`) already uses this exact shape, so it is proven internally; `serve`/`serve_listener` should be reimplemented on top of `into_shared()` + `accept` + `serve_connection` rather than carrying a second connection path. A deeper generalization, making the connection loop generic over the WS transport (`S: AsyncRead + AsyncWrite + Unpin + Send + 'static`, as `proxy_connection` already is) so an already-upgraded socket from an HTTP framework can be passed in directly, would also serve embedders already built on such a framework, but it is a larger refactor; the accepted-`TcpStream` form above covers the peek-and-route case without it. Priority: optional, only needed for one-port co-hosting.

**Tasks.**

- Add `WebSocketServer::into_shared(self) -> SharedWebSocketServer`, where `SharedWebSocketServer` wraps the `Arc<ConnectionConfig>` that `serve_listener` builds today (it already carries `offreader_limit`). `Clone` is an `Arc` clone; the handle is `Send + Sync + 'static`.
- Add `WebSocketServer::accept(stream, path) -> Result<WebSocketStream<TcpStream>, RepeError>` as a thin public wrapper over the existing `accept_repe_websocket`.
- Add `SharedWebSocketServer::serve_connection(&self, ws) -> Result<(), RepeError>` running one connection via the existing `handle_connection_with_config`.
- Reimplement `serve_listener` on top of `into_shared()` + `accept` + `serve_connection` so there is a single connection path. `serve_one` already demonstrates the destructure-once, serve-per-connection shape.

**Acceptance.** An embedder can `accept` a `TcpStream` then `serve_connection` it without the built-in accept loop; `ConnectionConfig` is built once per `into_shared`, not per connection; `with_peer_registry` connect/disconnect hooks still fire on a connection served this way.

**Test.** Drive a connection through `into_shared()` + `accept` + `serve_connection` directly (mirroring `serve_one`) and round-trip a request; assert a `PeerRegistry` attached via `with_peer_registry` observes the peer.

## Feature 5 (minor): expose `CallContext` to middleware

`Next` already carries the `CallContext` internally and threads it to the leaf handler, but the field is private and there is no accessor, so a `Middleware` cannot read the calling peer. Add:

```rust
impl<'a> Next<'a> {
    pub fn ctx(&self) -> Option<&CallContext<'a>>;   // or peer() -> Option<&PeerHandle>
}
```

Purely additive. Priority: low, since `with_json_ctx` / `with_typed_ctx` already give handlers the peer; this is only for cross-cutting middleware that wants it.

**Tasks.**

- Add `pub fn ctx(&self) -> Option<&CallContext<'a>>` on `impl<'a> Next<'a>`, returning `self.ctx` (the field already exists: `Next.ctx` in `src/server.rs`).
- Optionally add `pub fn peer(&self) -> Option<&PeerHandle>` as sugar for `self.ctx.and_then(|c| c.peer())`.

**Acceptance.** A middleware can read the calling peer via `next.ctx()` (or `next.peer()`); middleware that ignores it is unaffected; `Next::run` still threads ctx to the leaf as today.

**Test.** Register a middleware that records whether `next.ctx().and_then(|c| c.peer())` is `Some`, drive a request over the WebSocket server (peer present), and assert it observed the peer. The existing `middleware_preserves_ctx_across_pipeline` test is a good template.

## Backward compatibility

Features 1, 3, 4, and 5 are additive. Existing `serve`, `with_json`, and `with_typed` callers compile and run unchanged. Off-reader dispatch is opt-in per handler and defaults to inline (`Execution::Inline`). Feature 2 is additive. None of these change the wire format or any frame shape.

## Doc cleanup (resolved on main)

These were live `repe::stream` doc bugs when this note was written against the 2.5.1 tag; they were fixed in commits 7c7035e and 9bd1d60, which are now ancestors of this document on `main`. Kept as a record so the history is legible, not as a TODO.

- `docs/streaming.md`'s chunk-loop sketch had drifted from the shipped API three ways: `push_replay` was called with three args (missing the logical `data_len`), `ReconnectOutcome::ResumeReady` was matched as a unit variant, and the sketch called a nonexistent `take_pending_resume()`. Fixed in 7c7035e — it now passes four args, binds `ResumeReady(resume)`, and replays via `replay_chunks_from`.
- `src/stream.rs`'s module doc named a downstream consumer; genericized in 7c7035e to point at a transport's sink (a `PeerHandle` from the built-in `WebSocketServer`).
- Two adjacent `cargo doc` warnings — an unresolved `[PeerHandle]` link in `registry.rs` and a redundant link target in `lib.rs` — were fixed in 9bd1d60; `cargo doc --no-deps` is now warning-clean.

## Resolved design decisions

These were the open questions; each is now settled with its rationale, and the feature sections above reflect the outcome.

1. **Off-reader variant naming: `_blocking`.** Use `with_json_blocking`, `with_json_ctx_blocking`, `with_typed_blocking`, `with_typed_ctx_blocking`. The suffix follows the dominant Rust idiom (`tokio::task::spawn_blocking`, `reqwest::blocking`), where `blocking` describes the callee — "runs on the blocking pool, may park" — which is precisely the contract: the handler runs under `spawn_blocking` and a `repe::stream` producer parks on a `Condvar`. Rejected: `_detached` (wrong — the response still returns through the writer, so it is not fire-and-forget), `_offreader` (leaks the reader/writer-task vocabulary an embedder has no reason to learn), `_spawned` (says nothing about blocking, vague about where). The only ambiguity — whether `blocking` refers to the reader or the handler — is resolved by ecosystem precedent and rustdoc; it never means "blocks the reader."

2. **TCP `Server`/`AsyncServer`: stay inline, WebSocket-only.** The motivation is absent on TCP. The TCP servers are strict request/response with a single read-then-write loop and no notify/push primitive: `PeerHandle`/`PeerSink` are constructed only on the WebSocket accept path, and TCP dispatch always uses `CallContext::detached`, so `ctx.peer()` is always `None` and a producer would have nothing to push to. The TCP loop also owns its `BufWriter` directly rather than behind a writer task and outbound channel, so an off-reader response would race the reader's own writes with no serialization. Off-reader on TCP would therefore require first rebuilding the TCP path into the WebSocket server's reader/writer-task model — a large change with no consumer. `Execution::Inline` is the default and the TCP path never consults it. Revisit only when both a TCP push primitive and a concrete TCP windowed-transfer consumer exist.

3. **Concurrency cap: yes, finite default, reject (never block the reader) on saturation.** This pushes back on the original "unbounded for parity with hand-rolled spawning": the hand-rolled spawn is exactly what this feature replaces *because* it has no safe bound, so adopting its missing bound as the default reproduces the footgun the feature exists to remove. The exposure is also broader than per-connection framing implies — `spawn_blocking` draws from tokio's process-global blocking pool (512 threads by default), and a producer parked in `wait_for_credit` holds its thread for the transfer's lifetime, so a few abusive connections can starve every other blocking task in the process. Decision: a per-connection semaphore via `with_offreader_limit(n)` with a small finite default (16 is a reasonable start); `0`/`None` opts back into unbounded for embedders who have sized their runtime deliberately. On saturation the reader does not block on the semaphore (that would stall the ACK/cancel frames the in-flight transfers need to finish and free a slot — the same deadlock class this feature removes); it `try`-acquires and, on failure, returns an error and keeps reading. The error *code* is itself worth surfacing: `ErrorCode` defines protocol codes `0..=7` and then `ApplicationErrorBase = 4096` (`src/constants.rs:15-26`), with nothing for "unavailable/overloaded," so the only option today is an application-space code a client cannot tell apart from a `4096` an ordinary handler returned, and disambiguating on the message string is fragile. This is a real REPE **protocol gap**, and per the project's friction-reporting practice it is worth flagging upstream: off-reader saturation — and the panic case in Feature 1, which wants an "internal error" code just as badly — is the first feature that needs reserved "unavailable"/"internal" codes in the `0..4096` range. Durable fix: propose those at the spec level. Interim: pick one application code, document its exact meaning, and treat the ambiguity as known. Separately, document that the per-connection cap bounds fairness, not the global pool: embedders expecting many concurrent streaming connections should raise `max_blocking_threads` or use a dedicated runtime.

4. **`serve_connection` shape: a `SharedWebSocketServer` handle from `into_shared(self)` — not `&self`, not both.** `into_shared` builds `Arc<ConnectionConfig>` exactly once; the handle is a cheap `Arc` clone, `Send + Sync + 'static`, so it drops straight into `tokio::spawn(async move { shared.serve_connection(ws).await })` per accepted connection — exactly the peek-and-route co-hosting loop's need. It also keeps `WebSocketServer` a pure builder, consumed at the `into_shared` boundary, rather than a half-built/half-frozen object. The existing `serve_one` test helper (`src/websocket_server.rs:586`) already uses this exact pattern (destructure into `ConnectionConfig`, `Arc`-wrap once, serve per connection), so it is proven internally. `accept` stays an associated function (it needs only the path). Rejected: a bare `&self serve_connection` (forces either a per-call `ConnectionConfig` rebuild — re-allocating the hook vectors every connection — or storing the prebuilt config inside `WebSocketServer`, muddying the builder); offering "both" is redundant surface for one capability.

5. **Feature 2 `StreamSource`: deferred, with a defined trigger.** Build it only when a second, distinct windowed-transfer consumer of the built-in server exists, so the trait is designed against more than one transfer shape; Feature 1 plus the existing `repe::stream` surface fully covers a single consumer. The one shape question to settle when it is built is pull vs push: because the producer emits only under credit, a pull contract (the server asks the source for the next chunk when the window has room) composes with backpressure better than a source that pushes on its own clock, and it must expose a seek so replay/resume can re-emit from an arbitrary offset. No trait is committed until then.
