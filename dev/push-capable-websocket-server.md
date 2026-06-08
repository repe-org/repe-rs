# Push-Capable WebSocket Server

## Status

Shipped in 2.5.0: `WebSocketServer::with_peer_registry`, the per-connection outbound channel + reader/writer task split, `broadcast_notify_*` helpers, and the `on_peer_connect` / `on_peer_disconnect` lifecycle hooks. The later embedder-keyed-lookup and handshake additions landed in 3.2.0 (see `http-cohosting-and-keyed-peer-registry.md`). Retained as the design record behind the push surface.

## Summary

Extend `repe::websocket_server::WebSocketServer` so embedders can push server-initiated REPE notifies to connected clients without rebuilding the per-connection transport loop. Today the built-in server is request-response only: every project that needs server push (status broadcasts, live state mirroring, progress updates) has to replace `WebSocketServer` with a hand-rolled equivalent that pairs the reader with an outbound channel and tracks live peers.

This document is the finalized design. It proposes:

1. An additive opt-in `PeerRegistry` API exposed on `WebSocketServer`.
2. A per-connection bounded outbound channel plus a split reader/writer task pair inside `handle_connection`, so notifies sent through a peer's `PeerHandle` reach the wire alongside request responses.
3. Convenience broadcast helpers on `PeerRegistry` (`broadcast_notify_json`, `broadcast_notify_beve`, `broadcast_notify_utf8`, `broadcast_notify_raw`).
4. Two lifecycle hooks (`on_peer_connect`, `on_peer_disconnect`) that the registry composes from. Both are usable directly for embedders that want fine-grained control.

Existing code that calls `WebSocketServer::new(router).serve(addr, path)` keeps compiling and runs unchanged. The push path is strictly opt-in.

## Motivation

The crate already exposes the right *abstractions* for server-initiated push:

- `PeerHandle` / `PeerSink` / `NotifyBody` (`src/peer.rs`) describe "where to push a notify."
- `WebSocketClient::subscribe_notifies()` (`src/websocket_client.rs`) lets clients receive them.
- `Message::builder().notify(true)` builds notify frames.

What's missing is a *server* that actually delivers them. `WebSocketServer::handle_connection` (`src/websocket_server.rs:104`) splits the WebSocket stream into a writer and reader, then runs a single loop that reads a frame, routes it, and writes the response on the same task. There is no outbound queue, no `PeerHandle` wired into the per-connection scope, and no way for an external task (a status broadcaster, a job-progress watcher, a config-change publisher) to enumerate live peers and push them a notify.

Embedders that need push therefore replace `WebSocketServer` entirely. The replacement is mechanical: an outbound `mpsc::Sender` per connection, a writer task draining that channel, a reader task routing requests and forwarding responses through the same channel, and a per-connection `PeerHandle` registered into a shared map at accept and removed at close. Centralizing this in repe collapses the duplication and removes a foot-gun: the hand-rolled loops vary in how they handle backpressure, slow consumers, and disconnect detection, and not all of them get those choices right.

## Current Gap

`WebSocketServer::handle_connection`:

```rust
async fn handle_connection(
    ws_stream: WebSocketStream<TcpStream>,
    router: Router,
) -> Result<(), RepeError> {
    let (mut writer, mut reader) = ws_stream.split();
    loop {
        let frame = match reader.next().await { ... };
        let request = decode_request_frame(frame)?;
        if let Some(response) = route_request(&router, &request) {
            writer.send(WsMessage::Binary(response.to_vec())).await?;
        }
    }
}
```

Constraints:

- The writer is captured by the request-handling task; no other task can write.
- `route_request` (`src/server_request.rs:5`) takes only `&Router` and `&Message`; no peer context flows to handlers, so even if the writer were shared, a handler couldn't reach a `PeerHandle` for in-handler notifies.
- There is no observable connect/disconnect signal for external code.

Result: embedders re-implement the transport with their own outbound channel and peer book-keeping. Every implementation looks the same.

## Final Design

All additions are gated on the existing `websocket` feature.

### `PeerRegistry` (new, in `src/peer.rs`)

```rust
/// Live set of peers attached to one server instance.
///
/// Inserted on accept, removed on close. Cheap to clone (`Arc` inside)
/// so background tasks can hold a handle for broadcasting. The registry
/// is transport-agnostic: anything that produces `PeerHandle`s can feed
/// it, not just `WebSocketServer`.
#[derive(Clone, Default)]
pub struct PeerRegistry {
    inner: Arc<std::sync::Mutex<HashMap<PeerId, PeerHandle>>>,
}

impl PeerRegistry {
    pub fn new() -> Self;

    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    /// Snapshot of currently-connected peer handles. Cloned out under
    /// the lock so callers can iterate without holding it.
    pub fn peers(&self) -> Vec<PeerHandle>;

    /// Lookup by id. `None` if the peer is no longer connected.
    pub fn get(&self, id: PeerId) -> Option<PeerHandle>;

    /// Insert a peer. Overwrites any previous entry under the same id
    /// (caller responsibility: ids should be unique).
    pub fn insert(&self, peer: PeerHandle);

    /// Remove a peer. Returns the removed handle if it was present.
    pub fn remove(&self, id: PeerId) -> Option<PeerHandle>;

    /// Broadcast a JSON-bodied notify to every connected peer.
    ///
    /// The body is encoded **once**; the encoded bytes are cloned into
    /// each peer's outbound channel. The returned map gives a per-peer
    /// result so callers can prune dead peers or surface backpressure.
    /// A slow peer cannot block delivery to the others: each send is
    /// non-blocking (`try_send`-shaped). The registry's `Mutex` is held
    /// only long enough to snapshot the peer map to a `Vec<PeerHandle>`,
    /// not across the per-peer sends.
    pub fn broadcast_notify_json<P, T>(
        &self,
        path: P,
        body: &T,
    ) -> HashMap<PeerId, Result<(), PeerSendError>>
    where
        P: AsRef<str>,
        T: Serialize;

    /// BEVE-bodied broadcast. Same semantics as `broadcast_notify_json`.
    pub fn broadcast_notify_beve<P, T>(
        &self,
        path: P,
        body: &T,
    ) -> HashMap<PeerId, Result<(), PeerSendError>>
    where
        P: AsRef<str>,
        T: Serialize;

    /// UTF-8-bodied broadcast. Pass plain text, advertised with
    /// `BodyFormat::Utf8`.
    pub fn broadcast_notify_utf8<P, S>(
        &self,
        path: P,
        text: S,
    ) -> HashMap<PeerId, Result<(), PeerSendError>>
    where
        P: AsRef<str>,
        S: AsRef<str>;

    /// Raw-body broadcast. The caller supplies the body bytes and
    /// the `BodyFormat` tag the wire should advertise.
    pub fn broadcast_notify_raw<P>(
        &self,
        path: P,
        body_format: BodyFormat,
        body: &[u8],
    ) -> HashMap<PeerId, Result<(), PeerSendError>>
    where
        P: AsRef<str>;
}
```

Notes:

- `PeerSendError` already has the right variants (`src/peer.rs:48-60`): `Disconnected`, `Full`, `Other(String)`. No new error type is introduced.
- Broadcast helpers wrap the existing `PeerHandle::send_notify(method, NotifyBody)` (`src/peer.rs:155`). Encoding (`serde_json::to_vec` / `beve::to_vec`) happens once on the caller's task, before the peer iteration begins. Per-peer cost is one `Vec<u8>::clone()`, not a re-encode.
- For very large broadcast bodies fanned out to many peers (e.g. multi-MiB payloads to hundreds of peers), the clone storm becomes the dominant cost. That's deliberately out of scope for this phase; future work tracks an `Arc<[u8]>`-based `NotifyBody` variant that the broadcast helpers can share.

### `WebSocketServer` extension (in `src/websocket_server.rs`)

```rust
impl WebSocketServer {
    /// Attach a registry. Each accepted connection inserts its
    /// `PeerHandle`; the entry is removed when the connection closes
    /// (clean close, transport error, peer drop, or panic in a
    /// handler). The registry is `Arc`-shared so background tasks can
    /// clone it and broadcast.
    ///
    /// Sugar over `on_peer_connect` + `on_peer_disconnect`. Calling
    /// `with_peer_registry` does not exclude additional
    /// `on_peer_connect` / `on_peer_disconnect` callbacks; hooks
    /// compose in registration order.
    pub fn with_peer_registry(self, registry: PeerRegistry) -> Self;

    /// Run `f` from the per-connection task on accept, just after the
    /// `PeerHandle` is built and *before* any reader/writer activity.
    /// Notifies queued from `f` via `peer.send_notify(...)` are
    /// guaranteed to land on the wire before any response.
    ///
    /// The callback runs synchronously on the accept task: it must
    /// not block. Offload work to a channel or `tokio::spawn` if
    /// needed.
    pub fn on_peer_connect<F>(self, f: F) -> Self
    where
        F: Fn(PeerHandle) + Send + Sync + 'static;

    /// Run `f` when the connection closes (any exit path: clean
    /// Close frame, transport error, handler panic, dropped task).
    /// Fires exactly once per accepted connection.
    pub fn on_peer_disconnect<F>(self, f: F) -> Self
    where
        F: Fn(PeerId) + Send + Sync + 'static;

    /// Per-connection outbound channel capacity. Defaults to 256.
    /// A full channel returns `PeerSendError::Full` from
    /// `PeerHandle::send_notify`; the embedder decides whether to
    /// retry, drop, or prune.
    pub fn with_outbound_capacity(self, capacity: usize) -> Self;
}
```

`with_peer_registry` is internally implemented as:

```rust
let r = registry.clone();
self.on_peer_connect(move |peer| r.insert(peer))
    .on_peer_disconnect({
        let r = registry;
        move |id| { r.remove(id); }
    })
```

so the two-callback shape is the canonical entry point and the registry is a convenience built on top.

### Usage shape

```rust
use repe::PeerRegistry;
use repe::server::Router;
use repe::websocket_server::WebSocketServer;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let router = Router::new().with_json("/ping", |_| {
        Ok(serde_json::json!({ "ok": true }))
    });

    let peers = PeerRegistry::new();
    let server = WebSocketServer::new(router).with_peer_registry(peers.clone());

    // Background task pushes a notify whenever some shared state changes.
    let publisher = peers.clone();
    tokio::spawn(async move {
        loop {
            let snapshot = wait_for_state_change().await;
            let results = publisher.broadcast_notify_json("/state/changed", &snapshot);
            for (id, res) in results {
                if let Err(err) = res {
                    eprintln!("peer {id}: {err}");
                }
            }
        }
    });

    server.serve("0.0.0.0:8081", "/repe").await
}
```

## Implementation Plan

### Phase 1: Per-Connection Outbound Channel

Replace the single-task `handle_connection` with a reader/writer pair coordinated by a bounded `tokio::sync::mpsc` channel of `Message`.

1. Before `ws_stream.split()`, build `(outbound_tx, outbound_rx) = mpsc::channel::<Message>(capacity)` using the server's `outbound_capacity` (default 256, see Open Questions).
2. Split the connection into two cooperating tasks under one `tokio::select!`:
   - **Reader loop**: reads frames, decodes requests, calls `route_request`, and pushes `Some(response)` into `outbound_tx`. On `outbound_tx.send(...).await` failure (writer task gone), exit the reader.
   - **Writer loop**: drains `outbound_rx` and writes each `Message` to the wire as a binary WebSocket frame (`WsMessage::Binary(msg.to_vec())`). On wire-write error, exit the writer.
3. Closing sequence (deterministic, never leaks a task):
   - **Inbound EOF / Close frame / transport read error**: reader exits its loop; `outbound_tx` (held in the reader scope) is dropped; writer sees `recv() -> None`, drains remaining queued messages, sends a WebSocket Close frame, and exits.
   - **Outbound write error**: writer exits its loop; `outbound_rx` is dropped; the next reader `outbound_tx.send(...).await` returns `SendError` and the reader exits.
   - The connection task awaits both with `tokio::try_join!` (or sequenced `await`s) and propagates the first error.
4. The `WsPeerSink` holds a clone of `outbound_tx`. Sending a notify is `try_send` (non-blocking); the channel-closed case maps to `PeerSendError::Disconnected`, channel-full to `PeerSendError::Full`.

After Phase 1, behavior is observationally identical to today: only the reader pushes into the channel, so responses come out in the same order as requests, and there is no new public surface yet.

### Phase 2: Per-Connection `PeerHandle`

1. Introduce `pub(crate) struct WsPeerSink { tx: mpsc::Sender<Message> }` in `src/websocket_server.rs`. Keeping it crate-private discourages embedders from constructing parallel sinks against the same connection.
2. Implement `PeerSink` for `WsPeerSink`:
   - `send_notify(method, body)` builds the `Message` via `Message::builder().id(0).notify(true).query_str(method).query_format(QueryFormat::JsonPointer).body_bytes(body.into_bytes()).body_format(body.body_format()).build()` and calls `tx.try_send(msg)`.
   - `try_send` errors map: `TrySendError::Full(_) -> PeerSendError::Full`, `TrySendError::Closed(_) -> PeerSendError::Disconnected`.
   - `is_connected()` returns `!self.tx.is_closed()`.
3. Assign a `PeerId` per connection from an `Arc<AtomicU64>` stored on `WebSocketServer`, using `fetch_add(1, Ordering::Relaxed)`. Wrap is not a practical concern (a billion connections per second would still take ~580 years to exhaust `u64`); a `debug_assert!(id != PeerId::DETACHED.0)` is enough to flag the sentinel collision in tests without paying for it in release.
4. Build `PeerHandle::new(peer_id, Arc::new(WsPeerSink { tx: outbound_tx.clone() }))` immediately after the channel is created.

### Phase 3: Lifecycle Hooks + Registry

1. Add to `WebSocketServer`:
   - `on_connect: Vec<Arc<dyn Fn(PeerHandle) + Send + Sync>>`
   - `on_disconnect: Vec<Arc<dyn Fn(PeerId) + Send + Sync>>`
   - `outbound_capacity: usize`
   - `peer_id_counter: Arc<AtomicU64>`

   Hooks compose: `on_peer_connect` / `on_peer_disconnect` push onto the corresponding `Vec`, and the per-connection task invokes every entry in registration order. This is what lets `with_peer_registry` coexist with embedder-supplied logging / metrics callbacks instead of clobbering them.
2. Wire the hooks in `handle_connection`:
   - After building the `PeerHandle`, but **before spawning the reader/writer tasks**, call `on_connect(&peer)`. This is the structural guarantee that any notify queued from the hook is in `outbound_rx` before the writer task starts draining and before the reader task can produce its first response.
   - Wrap the per-connection scope in a `DisconnectGuard { peer_id, on_disconnect }` whose `Drop` impl runs the disconnect callback. The guard fires on any exit path (clean close, transport error, task abort, handler panic). This makes "fires exactly once per accepted connection" a property of `Drop`, not of careful manual sequencing.
3. Implement `with_peer_registry(r)` as the two-line sugar shown above.
4. Add `with_outbound_capacity(n: usize)` builder method. `n = 0` is rejected at runtime (bounded mpsc with capacity 0 has surprising semantics); the builder asserts `n >= 1`.

### Phase 4: Broadcast Helpers

1. Implement each `broadcast_notify_*` by:
   - Encoding the body **once** into a `Vec<u8>` (e.g. `serde_json::to_vec(body)?`). For helpers that take pre-encoded bytes (`broadcast_notify_raw`), this step is a no-op.
   - Snapshotting the peer map under the registry lock into `Vec<PeerHandle>`.
   - Releasing the lock.
   - For each handle: build a fresh `NotifyBody` cloning the encoded bytes, call `handle.send_notify(path, notify_body)`, and collect the result keyed by `PeerId`.
2. `broadcast_notify_*` are not `async`: `send_notify` is synchronous (`try_send` under the hood). The whole method is callable from sync code, futures, or `Drop` impls without forcing a runtime context.
3. Per-peer per-broadcast cost: one `Vec<u8>` clone (for the body). The query bytes are produced afresh per peer because each `Message` carries an owned `query: Vec<u8>` (`src/message.rs:8`); a future optimization could push `Arc<[u8]>` through the `Message` type but that's protocol-level and out of scope.

## Internal Changes Summary

- `src/peer.rs`: add `PeerRegistry` (and its tests).
- `src/websocket_server.rs`:
  - Replace the single-task `handle_connection` body with a reader + writer pair coordinated via `mpsc`.
  - Add `WsPeerSink`, lifecycle hook fields, `peer_id_counter`, and the new builder methods.
  - Add `DisconnectGuard` for `Drop`-based disconnect signaling.
- `src/lib.rs`: re-export `PeerRegistry` from the crate root (next to the existing `PeerHandle` / `PeerSink` exports at `src/lib.rs:72`).
- `docs/websocket.md`: add a "Server-Initiated Notifies" section showing the registry pattern.
- `CHANGELOG.md`: entry under `[Unreleased]` describing the additive API.

`src/server_request.rs` is **not** modified in this phase. Handlers still receive only `&Message`; the `PeerHandle` is reachable only through `PeerRegistry` (i.e. from outside the handler). In-handler notifies are deferred (see Future Work).

## Future Work (explicitly out of scope here)

1. **In-handler notifies.** `route_request` does not thread a `PeerHandle` into the handler today. Surfacing the calling peer's handle from a request handler requires a router/handler signature change (handlers receive a `CallContext` like the registry already does, see `Registry::dispatch_with_ctx` at `src/registry.rs:244`). This is a separate, larger change. Once Phase 1's outbound channel exists the wiring becomes mechanical: pass the per-connection `PeerHandle` into `route_request`, expand the handler trait to accept a `CallContext`, and provide a backward-compatible blanket impl that ignores the context for existing handlers. Tracked as a follow-up.
2. **HTTP transport push.** The same problem exists for any future `repe::http_server` but the answer is different (SSE / long-poll, not a symmetric duplex channel). Out of scope here.
3. **Zero-clone broadcast.** A future `NotifyBody::Shared(Arc<[u8]>, BodyFormat)` variant plus an `Arc<[u8]>`-aware `Message` would let one broadcast share one allocation across N peers. Not needed for the bring-up; revisit when a real workload demands it.
4. **WASM client.** `WasmClient::subscribe_notifies` is already a counterpart; no changes needed on the client side.

## Test Matrix

All new tests live next to the existing `websocket_server` tests (`src/websocket_server.rs:202-273`) or in a new integration test file `tests/websocket_push.rs` if they need multi-client setups.

### Unit / integration

| Scenario | Expectation |
|---|---|
| `WebSocketServer::new(router).serve(...)` with no registry / hooks | Behaves identically to today. Request-response roundtrips; the existing `websocket_server_roundtrip` test still passes byte-for-byte. |
| Single client subscribes; server broadcasts via registry | The client's `subscribe_notifies()` receiver yields the notify with the right `query` path and body. |
| Multiple clients; broadcast to all | Each client receives the notify exactly once. Registry length matches the number of live clients before and after the broadcast. |
| Client A has an in-flight request; broadcast lands during handler execution | Client A sees one notify (on `subscribe_notifies()`) and one response (on the pending call future). Both decode correctly. |
| Slow consumer; per-peer outbound channel is full | `broadcast_notify_*` returns `PeerSendError::Full` for that peer; other peers still receive. Registry size unchanged (peer not auto-evicted). |
| Client disconnects mid-broadcast | Subsequent broadcasts return `PeerSendError::Disconnected` for that peer; the disconnect hook fires and removes it from the registry; registry size decrements. |
| Connect hook calls `peer.send_notify(...)` before the connection is "live" | The client's first received frame on that connection is the queued notify, *not* a request response. Verified by a request/response round-trip after the notify lands. |
| Handler panic | Disconnect hook still fires (Drop guard); peer is removed from the registry. The panic does not poison the server. |
| Mixed notify + request traffic at high rate | No frame interleaving / corruption (each WebSocket binary message is one whole REPE message). Use `Message::from_slice_exact` on the client side to assert. |
| `with_outbound_capacity(0)` | Builder rejects at runtime with a clear panic message. (We use `assert!` rather than a `Result` here because misconfiguration is a programmer error, not a runtime condition.) |
| Two `on_peer_connect` callbacks chained | Both fire, in registration order, on every connection. |

### Property / fuzz

- The existing fuzz harness for `Message::from_slice_exact` is unaffected.
- **New**: round-trip property test that pushes random `NotifyBody` payloads (Json / Beve / Utf8 / Raw with arbitrary `BodyFormat`) through the outbound channel and verifies they survive `WebSocketClient::subscribe_notifies()` unchanged: header `notify` flag set, `body_format` preserved, `query` path equal, body bytes equal.

## Compatibility & Migration

- **Source-compatible.** Existing `WebSocketServer::new(router).serve(...)` callers compile and run unchanged. Every new method is opt-in.
- **Wire-compatible.** No protocol changes. Notify frames are the same shape they've always been (`Message::builder().notify(true)`).
- **Embedder migration.** A project that has its own hand-rolled WebSocket server for push can replace the boilerplate with `WebSocketServer::with_peer_registry`. The migration drops several hundred lines of code and gains the shared backpressure / lifecycle / disconnect-on-panic handling.

## Exit Criteria

1. Phases 1 to 4 implemented and unit-tested per the matrix above.
2. CI green: the existing `websocket_server_roundtrip` and `websocket_proxy_forwards_raw_messages` tests still pass.
3. `docs/websocket.md` gains a "Server-Initiated Notifies" section that walks through the `PeerRegistry` pattern with a runnable snippet matching the "Usage shape" example.
4. `CHANGELOG.md` gets an `[Unreleased]` entry describing the additive API (`PeerRegistry`, `WebSocketServer::with_peer_registry`, the two lifecycle hooks, `with_outbound_capacity`).
5. No regressions in any existing WebSocket integration test.
6. `cargo doc --features websocket` builds without warnings; `cargo clippy --features websocket --tests` is clean.

## Open Questions, Resolved

These are the questions raised in earlier drafts of this document, now with concrete decisions.

### 1. Channel type: `tokio::sync::mpsc` vs `flume`

**Decision: `tokio::sync::mpsc`.**

The crate already depends on tokio for every async path (`src/websocket_client.rs:14` uses `tokio::sync::mpsc` for the client-side notify channel). Adding `flume` would mean a new dependency for marginal benefit. `tokio::sync::mpsc` also gives `try_send`, `is_closed`, and `Sender::same_channel` semantics that match what the client already relies on, keeping the mental model consistent across the codebase.

### 2. Default outbound capacity

**Decision: 256, fixed at compile time. No env var.**

256 messages is comfortable for normal use: a slow client briefly stalling at, say, 100 ms per drain still gets a multi-second backlog before back-pressure trips. An env var (`REPE_WS_OUTBOUND_CAP`) was considered for ops-tuning, but rejected: the right knob for an embedder is per-server (some servers stream high-rate notifies, others don't), so `with_outbound_capacity(n)` already covers it. Process-wide env vars muddy multi-server deployments and create silent behavior shifts on misconfiguration.

### 3. Backpressure policy on a full channel

**Decision: `try_send` + return `PeerSendError::Full`.**

The alternatives are:
- `send().await` (block the broadcaster): one slow peer stalls the broadcast to everyone else, which defeats the point of broadcast.
- Drop silently on full: corrupts streaming notify protocols where each notify is significant (chunked transfers).

`try_send` + structured error keeps the broadcaster fast and lets the embedder choose: retry on the next tick, prune the peer, or escalate. This also matches how `PeerSink::send_notify` is documented (`src/peer.rs:111-122`).

### 4. Lifecycle entry point: two closures vs a `PeerLifecycle` trait

**Decision: two closures (`on_peer_connect`, `on_peer_disconnect`).**

Closures keep the call site one line and compose well (`with_peer_registry` itself is built from them). A trait adds a type that most embedders would implement once for their `Arc<MyState>` anyway; the closure form lets them reach into their state directly without an extra newtype. If a future use case wants paired lifecycle objects, we can add a trait later as additional sugar without touching the closure form.

### 5. Visibility of `WsPeerSink`

**Decision: `pub(crate)`.**

`PeerRegistry` plus the two lifecycle hooks cover every embedder use case identified so far. Exposing `WsPeerSink` would let an embedder build a parallel sink against the same WebSocket stream, which is a foot-gun (now you have two writers to the same socket). Keeping it crate-private removes that hazard. If a future use case actually needs to construct a `WsPeerSink` from outside (it doesn't today), we can flip the visibility additively.

### 6. Should the broadcast helpers be `async`?

**Decision: no, they are synchronous.**

`PeerSink::send_notify` is synchronous (it returns `Result<(), PeerSendError>`, not a future), because the underlying `try_send` is non-blocking. Making the broadcast helpers `async` would force callers to enter a runtime context for what is in practice a tight loop of synchronous sends. Sync also lets the helpers be called from `Drop` impls, signal handlers, and other non-async contexts.

### 7. What happens to a queued notify if the connection closes between enqueue and write?

**Decision: dropped silently.**

When the writer task exits (transport error, peer Close), `outbound_rx` is dropped. Any messages still in the channel are discarded. This is consistent with the existing client behavior (`src/websocket_client.rs:798` drains pending requests on close). The disconnect hook fires after the channel is drained, so embedders that want at-most-once delivery semantics can track per-peer state from there.

The alternative ("flush before close") was rejected because the network may already be unusable; flushing into a dead socket would just block until the OS noticed.
