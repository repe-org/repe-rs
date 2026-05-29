# WebSocket Transport

Enable the `websocket` feature for the native `WebSocketClient`, `WebSocketServer`, and `proxy_connection`. Enable `websocket-wasm` for a browser `WasmClient` on `wasm32-unknown-unknown`.

Each REPE message maps to exactly one WebSocket binary message. WebSocket decoding enforces exact message length within each bounded binary frame.

## Native Client

```rust
use repe::WebSocketClient;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = WebSocketClient::connect("ws://127.0.0.1:8081/repe").await?;
    let pong = client.call_json("/ping", &json!({})).await?;
    assert_eq!(pong["pong"], true);
    Ok(())
}
```

## Receiving Server-Pushed Notifies

`WebSocketClient::subscribe_notifies()` returns a tokio mpsc receiver that yields every inbound `Message` whose `notify` header flag is set. The response loop checks the notify flag *before* the request/response correlation map, so server-pushed notifies never collide with an in-flight request that happens to share the same id.

```rust
use repe::WebSocketClient;
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = WebSocketClient::connect("ws://127.0.0.1:8081/repe").await?;
    let mut notifies = client.subscribe_notifies()?;
    tokio::spawn(async move {
        while let Some(msg) = notifies.recv().await {
            let path = msg.query_str().unwrap_or("");
            let body: Value = msg.json_body().unwrap_or(Value::Null);
            println!("notify {path}: {body}");
        }
    });
    let _ = client.call_json("/start", &serde_json::json!({})).await?;
    Ok(())
}
```

The receiver yields raw `Message` values; decode the body using `Message::json_body::<T>()`, `beve::from_slice(&msg.body)`, or `MessageView::from_slice(&frame_bytes)` as appropriate for the wire `body_format`.

### Subscription rules

- At most one subscriber may be active at a time. If a live subscription already exists, `subscribe_notifies` returns `Err(AlreadySubscribed)` without disturbing the existing receiver. This matters because `WebSocketClient` is `Clone`: the loud-replace contract keeps two holders of the same client from silently stealing each other's subscription. Call `unsubscribe_notifies()` first to take over.
- A stale slot whose receiver was already dropped does not block a new subscription; in that case `subscribe_notifies` silently installs the new sender.
- Notifies that arrive while no subscriber is registered are silently dropped. Logging every drop would avalanche under high-rate notifies (e.g. server-pushed binary chunks).

### Backpressure

The notify channel is unbounded by design. The transport read loop pushes every inbound notify into the channel without backpressure, so a slow consumer plus a high-rate producer will grow the buffer until the process OOMs. Application-level backpressure (e.g. ACK windows in a chunk protocol; see [streaming.md](streaming.md)) is the right fix; the consumer must drain the receiver promptly.

A bounded variant is not offered: dropping notifies on overflow corrupts chunk streams, and blocking the read loop on overflow stalls the request/response correlation path that shares the same socket.

## Server-Initiated Notifies

`WebSocketServer` can push REPE notifies to connected clients without an embedder having to rebuild the per-connection transport loop. Attach a `PeerRegistry` (or register raw connect/disconnect hooks) and the server tracks each accepted peer's outbound channel for you.

```rust
use repe::PeerRegistry;
use repe::server::Router;
use repe::websocket_server::WebSocketServer;
use serde_json::json;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let router = Router::new().with_json("/ping", |_| Ok(json!({ "ok": true })));

    let peers = PeerRegistry::new();
    let server = WebSocketServer::new(router).with_peer_registry(peers.clone());

    // Background task fans out a notify to every connected client.
    let publisher = peers.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = publisher.broadcast_notify_json("/heartbeat", &json!({ "t": 1 }));
        }
    });

    server.serve("0.0.0.0:8081", "/repe").await
}
```

### `PeerRegistry`

`PeerRegistry::new()` builds an empty, cloneable handle. `with_peer_registry` wires accept/close to `insert`/`remove`. Background tasks clone the registry and broadcast.

Per-peer broadcast helpers all encode the body once on the caller's task and clone the encoded bytes into each peer's outbound channel:

- `broadcast_notify_json<P, T: Serialize>(path, body)` — JSON body.
- `broadcast_notify_beve<P, T: Serialize>(path, body)` — BEVE body.
- `broadcast_notify_utf8<P, S: AsRef<str>>(path, text)` — UTF-8 body.
- `broadcast_notify_raw<P>(path, body_format, body_bytes)` — raw bytes with the caller-chosen `BodyFormat` tag.

Each helper returns a `HashMap<PeerId, Result<(), PeerSendError>>` so callers can prune dead peers (`PeerSendError::Disconnected`) or surface backpressure (`PeerSendError::Full`).

### Addressing peers by an embedder key

`PeerId` is the registry's canonical, server-minted key: it is what `with_peer_registry` auto-inserts and what the `broadcast_notify_*` result maps are keyed by. When your own client identity is something else — a UUID/token from the handshake, a domain id — attach it as a secondary **alias** instead of maintaining a parallel `key -> PeerId` map by hand:

- `alias<K: Into<String>>(peer_id, key)` associates an embedder key with an already-inserted peer. Returns `false` if the peer is gone (no dangling alias is created). A peer may carry several aliases; a given key addresses at most one peer (re-aliasing moves it).
- `get_by<Q>(&key)` resolves an alias to its `PeerHandle` for a **targeted push** by your own identity (`peers.get_by("session-abc").map(|h| h.send_notify(...))`). The lookup is generic over the borrowed key form, like `HashMap::get`, so string aliases are queried with a `&str`.
- `key_for(peer_id)` / `aliases_for(peer_id)` map a `PeerId` **back** to your key(s). Use these to read a `broadcast_notify_*` result map by your own identity — e.g. to answer "which of *my* clients hit `PeerSendError::Full`?". `key_for` returns the first alias (the common single-alias case); `aliases_for` returns all of them.

`PeerId` stays canonical, so the auto-insert path is untouched and the whole feature is additive. Aliases are cleaned up automatically: `remove` (which `with_peer_registry` routes disconnect through) drops the peer's primary entry and every alias pointing at it, so a disconnect needs no extra bookkeeping. Aliases are owned `String`s; convert a non-string key with `to_string()`.

When the key rides in the upgrade request itself, derive it at connect time with [`on_peer_connect_with_handshake`](#lifecycle-hooks):

```rust
use repe::{PeerRegistry, NotifyBody, WebSocketServer};

let peers = PeerRegistry::new();
let server = WebSocketServer::new(router)
    .with_peer_registry(peers.clone())
    .on_peer_connect_with_handshake({
        let peers = peers.clone();
        move |peer, hs| {
            // e.g. a bearer token in the Authorization header
            if let Some(token) = hs.header("authorization") {
                peers.alias(peer.peer_id(), token);
            }
        }
    });

// Later, anywhere holding a registry clone — a targeted push by your key:
if let Some(handle) = peers.get_by("Bearer abc123") {
    let _ = handle.send_notify("/nudge", NotifyBody::Json(b"{}".to_vec()));
}
```

### Lifecycle hooks

`with_peer_registry` is sugar over two lower-level hooks:

```rust
let server = WebSocketServer::new(router)
    .on_peer_connect(|peer| { /* metrics: connection opened */ })
    .on_peer_disconnect(|id| { /* metrics: connection closed */ });
```

Both hooks compose: every registered closure fires in registration order, so `with_peer_registry` can coexist with logging or metrics callbacks instead of replacing them.

The connect hook runs synchronously on the accept task **before** the reader/writer tasks start processing traffic. A notify queued via `peer.send_notify(...)` from inside the hook is guaranteed to land on the wire before any response to a client request on that connection.

After the hook returns, the outbound channel is strict FIFO across both notifies and responses on the same connection: a notify pushed by a background broadcaster at time T arrives on the wire in the order it was enqueued relative to any handler response also enqueued at time T. Clients de-multiplex by the `notify` header flag (`WebSocketClient::subscribe_notifies()`); notifies do not "cut the line" past in-flight responses.

The disconnect hook fires from a `Drop` guard, so it runs on every exit path: clean Close frame, transport error, handler panic, or a panic in a *later* connect hook (the guard is built before the connect-hook loop). Exactly once per accepted connection.

`on_peer_connect_with_handshake(|peer, hs|)` is a sibling connect hook that additionally receives a `HandshakeContext` — the upgrade request's `path()`, `query()`, and `header(name)` (case-insensitive). Use it when your client identity rides in the handshake: derive your key from the request and `alias` the just-inserted peer (see [Addressing peers by an embedder key](#addressing-peers-by-an-embedder-key)). These hooks fire **after** the plain `on_peer_connect` hooks — so a `with_peer_registry` insert has already landed and the peer is present for `alias` — and still before any traffic is processed. The context is available wherever the handshake is: the built-in `serve*` loops, and the co-hosting `accept_with_handshake` + `serve_connection_with_handshake` (or `serve_connection_with_cancel_and_handshake`) path. A connection served through plain `serve_connection`, which carries no captured handshake, does not fire these hooks. If your key instead arrives in a request *body*, this hook is unnecessary — the handler already has both the key and the peer via `CallContext::peer`.

`PeerHandle::send_notify` returning `Ok(())` means the message was queued onto the outbound channel, not that it was written to the wire. During concurrent disconnect, a late-arriving send may queue successfully but be dropped when the writer task exits and the bounded receiver is released. This is intrinsic to async shutdown; treat `Ok` as best-effort delivery, not as confirmation.

### Error reporting

By default the server prints transport-level errors to stderr (`eprintln!`). To route them into your own `log` / `tracing` pipeline instead, register an `on_error` hook. It receives a `ConnectionError` — a typed taxonomy of the events that would otherwise hit stderr, plus the off-reader saturation rejection:

```rust
use repe::{ConnectionError, WebSocketServer};

let server = WebSocketServer::new(router).on_error(|err| match err {
    ConnectionError::Handshake(e)      => tracing::warn!(%e, "websocket handshake failed"),
    ConnectionError::Connection(e)     => tracing::info!(%e, "connection ended"),
    ConnectionError::HandlerPanic { method } => tracing::error!(%method, "off-reader handler panicked"),
    ConnectionError::Saturation { method }   => tracing::debug!(%method, "off-reader limit reached"),
});
```

Like the lifecycle hooks, `on_error` composes: every registered callback fires in registration order, on the task that detected the error (an accept task, a connection's reader, or an off-reader blocking thread), so it must not block. Registering at least one callback makes the callbacks the sole sink — the default stderr logging is suppressed. With no callback registered the server keeps its stderr behavior (and stays silent on saturation, which never logged).

`Handshake` and `Connection` errors are reported only by the built-in accept loops (`serve*`); a co-hosting embedder that owns its accept loop gets the connection error as the `RepeError` returned by `serve_connection` and handles it however it likes. `HandlerPanic` and `Saturation` are reported wherever the connection runs. `ConnectionError::error_code()` returns the wire `ErrorCode` that reached the client for the panic (`InternalError`) and saturation (`ResourceExhausted`) categories, and `None` for handshake / connection failures (which sent no REPE response).

### Backpressure on the server outbound channel

Each accepted connection allocates a bounded `tokio::sync::mpsc` channel for outbound messages (responses plus pushed notifies). The default capacity is `DEFAULT_OUTBOUND_CAPACITY` (256). Tune per server with `with_outbound_capacity(n)`.

When the channel is full, `PeerHandle::send_notify` (and thus `broadcast_notify_*`) returns `PeerSendError::Full` for that peer; other peers are unaffected. A slow consumer cannot stall delivery to the broadcast set. The embedder chooses the policy: retry on the next tick, prune the peer, or escalate.

### In-Handler Notifies

Request handlers can also push notifies back to the calling peer mid-request. Register a handler via `Router::with_json_ctx` (or `with_typed_ctx`) and the closure receives a `CallContext` carrying the `PeerHandle`:

```rust
use repe::server::Router;
use repe::websocket_server::WebSocketServer;
use repe::NotifyBody;
use serde_json::json;

let router = Router::new().with_json_ctx("/run", |ctx, _params| {
    if let Some(peer) = ctx.peer() {
        let _ = peer.send_notify(
            "/progress",
            NotifyBody::Json(serde_json::to_vec(&json!({ "stage": "begin" })).unwrap()),
        );
        // ... do work ...
        let _ = peer.send_notify(
            "/progress",
            NotifyBody::Json(serde_json::to_vec(&json!({ "stage": "end" })).unwrap()),
        );
    }
    Ok(json!({ "ok": true }))
});

let server = WebSocketServer::new(router);
```

Registry-backed handlers reach the same `CallContext`: wrap a closure with `repe::registry::WithContext` before registering it. `WebSocketServer` automatically calls `Registry::dispatch_with_ctx` so the registered callable receives the peer.

Middleware does not need to be aware of `CallContext` to forward it. `Next::run(req)` threads whatever the upstream caller attached, so existing middleware composes transparently with new context-aware handlers. Handlers registered via `with_json` / `with_typed` (the legacy non-`_ctx` variants) keep working unchanged; they simply do not receive the context. A cross-cutting middleware that *does* want the calling peer can read it through `next.ctx()` (or the `next.peer()` shortcut); both return `None` on peer-less transports.

Calls dispatched through the TCP transports (`Client`, `AsyncClient`, `Server`, `AsyncServer`) do not carry a peer today; `ctx.peer()` returns `None` for those. Context-aware handlers should treat `None` as the no-push case rather than relying on the peer always being present.

### Compatibility

The push path is strictly opt-in. Existing `WebSocketServer::new(router).serve(addr, path)` callers see no behavior change, no new error variants, and no protocol changes. Notify frames are the same shape they have always been (`Message::builder().notify(true)`).

Adding `handle_with_ctx` to `HandlerErased` is source-compatible because the trait provides a default implementation that delegates to `handle`. Existing implementors of `HandlerErased` (embedder custom handler types) compile unchanged.

## Off-Reader Handler Dispatch (`_blocking`)

By default every handler runs **inline** on the connection's reader task: the reader decodes a frame, runs the handler to completion, then decodes the next frame. That keeps strict per-connection, request-at-a-time ordering, but it means a handler that **blocks or parks** also stalls the reader — no further inbound frames on that connection are decoded until it returns. On a `current_thread` runtime it stalls the whole server.

That is fatal for a [`repe::stream`](streaming.md) producer. The producer parks in `wait_for_credit` until the receiver ACKs, and those ACKs arrive as inbound frames the *same* reader must decode. Drive the producer from an inline handler and the reader is parked inside it, the ACKs are never read, the credit window never reopens, and the transfer deadlocks at the first full window.

The `_blocking` constructors opt a route out of inline dispatch:

```rust
use repe::server::Router;

let router = Router::new()
    .with_json("/status", |_| Ok(serde_json::json!({ "ok": true })))  // inline, as usual
    .with_json_blocking("/download/begin", |_params| { /* may block / park */ Ok(serde_json::json!({})) })
    .with_json_ctx_blocking("/export", |ctx, _params| { /* ctx.peer() + may park */ Ok(serde_json::json!({})) })
    .with_typed_blocking::<Req, Resp, _>("/report", |req| { /* ... */ })
    .with_typed_ctx_blocking::<Req, Resp, _>("/stream", |ctx, req| { /* ... */ });
```

On the WebSocket server an off-reader handler runs on a blocking thread (`tokio::task::spawn_blocking`); the reader keeps decoding inbound frames (ACKs, cancels, resumes) while it runs or parks. The handler's response, if any, flows back through the same writer task on completion. The `_blocking` suffix describes the callee — it runs on the blocking pool and may park, exactly as in `tokio::task::spawn_blocking` — and never means "blocks the reader." The TCP `Server` / `AsyncServer` ignore the off-reader tag and always run inline (they have no peer and no push primitive).

Reach for `_blocking` when a handler **blocks or parks**: a `repe::stream` producer, a synchronous read of a large blob, a blocking database call. Leave fast request/response handlers inline.

### Ordering

Inline handlers keep strict per-connection, request-at-a-time order. Off-reader handlers run **concurrently** with subsequent inline handlers and with each other, so their responses interleave on the wire. Clients correlate responses to requests by the REPE message id (as they already do); do not assume per-connection serialization for off-reader routes. This is the same concurrency you would get by hand-rolling a worker thread, made first-class and opt-in.

### Concurrency cap and saturation

Off-reader dispatch is capped per connection by `with_offreader_limit(n)` (default `DEFAULT_OFFREADER_LIMIT`, 16). A permit is acquired before the handler is spawned and held for its whole life, so the count reflects actually-running handlers and a finished, panicked, or cancelled handler frees its slot on exit.

```rust
use repe::websocket_server::WebSocketServer;

let server = WebSocketServer::new(router).with_offreader_limit(32);  // 0 removes the cap
```

When the cap is reached the reader does **not** block waiting for a slot — that would stall the very ACK/cancel frames in-flight transfers need to finish and free a slot, recreating the deadlock. Instead a further off-reader request gets an immediate error response and the reader keeps reading; the client should retry. A handler **panic** is caught and likewise mapped to an error response, so the connection — which it shares with other concurrent transfers — survives rather than being torn down.

The saturation rejection and the caught panic carry **distinct** wire codes so a client can branch on them: a saturation rejection is `ErrorCode::ResourceExhausted` (retryable — back off and retry) and a caught panic is `ErrorCode::InternalError` (a real failure, not a normal result), both separate from the `ErrorCode::ApplicationErrorBase` an ordinary handler error returns. The two codes occupy the REPE-reserved `8..4095` range, so a client implementing automatic retry keys off the code alone — retry-on-busy, surface-on-error. (Both surfaced as `ApplicationErrorBase` before this distinction landed; a client that matched that code to detect saturation must update.)

`with_offreader_limit` bounds **per-connection fairness, not the global blocking pool.** `spawn_blocking` draws from tokio's process-wide blocking pool (512 threads by default), and a producer parked in `wait_for_credit` holds its thread for the whole transfer. Embedders expecting many concurrent streaming connections should raise the runtime's `max_blocking_threads` or use a dedicated runtime. The idle watchdog (`spawn_watchdog`, see [streaming.md](streaming.md)) reclaims both the blocking-pool thread and the permit from a transfer that goes silent.

### Cancellation on disconnect and shutdown

An off-reader handler can observe that its work has become pointless through its `CallContext`. `ctx.is_cancelled()` (non-blocking) returns `true`, and `ctx.cancelled()` (a future) resolves, once the calling peer disconnects or the server begins a [graceful drain](#draining-in-flight-connections). A long poll-loop or compute handler should check `is_cancelled()` at loop boundaries and return early, freeing its `spawn_blocking` thread and off-reader permit instead of running to completion:

```rust
let router = Router::new().with_json_ctx_blocking("/export", |ctx, _params| {
    for chunk in chunks {
        if ctx.is_cancelled() {
            return Ok(serde_json::json!({ "cancelled": true }));  // peer gone / shutting down
        }
        // ... produce and push the chunk via ctx.peer() ...
    }
    Ok(serde_json::json!({ "done": true }))
});
```

The signal is a never-cancelling no-op on peer-less transports (TCP servers, in-process dispatch), so the same handler is safe to register on a `Router` served over any transport — `is_cancelled()` simply stays `false` and `cancelled()` never resolves there.

**It does not wake a `wait_for_credit` park.** A `repe::stream` producer parked in `wait_for_credit` is blocked on a `Condvar`, not polling `is_cancelled()`, so the cancellation signal cannot reach it. That park is still woken only by `TransferControl::cancel`, which you drive from an `on_peer_disconnect` hook: the hook receives a `PeerId` while a `TransferRegistry` is keyed by your transfer id, so keep your own peer-to-transfer association and call `cancel` for each transfer that belonged to the departing peer. The two paths complement each other — poll-loop and compute handlers use the cancellation signal; credit-parked producers use `cancel`. With neither wired, a parked producer still wakes on the `deadline` passed to `wait_for_credit` (suggested `DEFAULT_BACKPRESSURE_TIMEOUT`, 30 s) or when the idle watchdog fires, so a departed peer's transfer holds its blocking-pool thread and off-reader permit for at most that long — the hold is bounded, not indefinite.

### One producer per transfer

Removing per-connection serialization also removes an implicit safety net. Inline, a second `/begin` for the same transfer id cannot start until the first returns; off-reader, two `begin` frames spawn two producers against one `TransferControl`, violating its single-producer assumption and racing the registry entry (the second `begin` overwrites it while both producers run). The concurrency cap bounds the *count* of off-reader tasks, not duplicate registration, so a `begin`-style handler **must reject (or de-duplicate) a transfer id that is already registered** — check `TransferRegistry::get` before `register` and return an application error if it is already present.

### Deferred: a first-class stream source

The wiring above — install a `TransferControl`, spawn the producer off the reader, and route inbound ack/cancel/resume through your own handlers — is deliberately left to the embedder. A higher-level `WebSocketServer` stream-source surface that owns all of it (you would supply only the byte source and the chunk method names, and `begin`/ack/cancel/resume would be handled internally) is **deferred** until a second distinct windowed-transfer consumer of the built-in server exists, so the trait is designed against more than one transfer shape rather than over-fit to one. Its prerequisites — cancellation-on-disconnect ([above](#cancellation-on-disconnect-and-shutdown)) and orderly drain ([below](#draining-in-flight-connections)) — now exist, so the surface can be designed against this consumer once a second appears. When it is built it should be *pull-based* (the server asks the source for the next chunk only when the credit window has room) and expose a seek so replay/resume can re-emit from an arbitrary offset. Until then, off-reader dispatch plus the [`repe::stream`](streaming.md) API fully cover a single consumer.

## Graceful Shutdown

`serve` and `serve_listener` run until the listener errors or the task is dropped. To stop accepting cleanly without tearing down the runtime, use `serve_with_shutdown` (which binds an address) or `serve_listener_with_shutdown` (which takes an already-bound listener); both `select!` between accepting connections and a shutdown future and return `Ok(())` once it resolves.

```rust
use repe::websocket_server::WebSocketServer;
use tokio::sync::oneshot;

let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
let server = WebSocketServer::new(router);
let handle = tokio::spawn(async move {
    server
        .serve_with_shutdown("0.0.0.0:8081", "/repe", async {
            let _ = shutdown_rx.await;
        })
        .await
});

// ... later, to stop accepting new connections:
let _ = shutdown_tx.send(());
let _ = handle.await;
```

Already-accepted connections are **not** awaited: each runs on its own detached task and continues until its peer disconnects or it errors, and the call returns as soon as the accept loop stops. Track connection lifetimes yourself (e.g. via a [`PeerRegistry`](#peerregistry)) if you need to drain them before exiting.

### Draining in-flight connections

`serve_with_shutdown` returns the instant the accept loop stops, so in-flight work on already-accepted connections is cut off at process exit. When connections carry multi-minute transfers, use the draining entry points instead — `serve_with_graceful_drain(addr, path, shutdown, drain_timeout)` and `serve_listener_with_graceful_drain(listener, path, shutdown, drain_timeout)`. On shutdown they stop accepting, **cancel** every in-flight off-reader handler (so handlers that poll [`ctx.is_cancelled()`](#cancellation-on-disconnect-and-shutdown) wind down promptly), then await the connection tasks for up to `drain_timeout` before aborting whatever remains.

```rust
server
    .serve_listener_with_graceful_drain(
        listener,
        "/repe",
        async { let _ = shutdown_rx.await; },
        Duration::from_secs(10),
    )
    .await?;
```

The server tracks every connection it accepts in a `JoinSet`, so you do not have to. `drain_timeout` is a backstop, not the common wait: cancellation winds cooperative handlers down in milliseconds, and the timeout only bounds connections whose handlers ignore cancellation or whose peers are slow to flush. Aborting a connection at the deadline tears down its writer task too, so the server-level `drain_timeout` supersedes the per-connection 5 s writer drain. A `_blocking` handler that never polls `is_cancelled()` still holds its `spawn_blocking` thread past the deadline (a blocking thread cannot be aborted) — the abort reclaims the connection's reader/writer, not such a handler, which is one more reason to poll the signal.

`examples/websocket_graceful_server.rs` is a runnable end-to-end demo of this section: a cancellation-polling off-reader handler, an `on_error` hook, and `serve_listener_with_graceful_drain` winding a connection down on shutdown. Run it with `cargo run --example websocket_graceful_server --features websocket`.

## One-Port Co-Hosting

To share a single TCP port between the REPE WebSocket endpoint and your own HTTP routes, drive connections yourself instead of using the built-in accept loop. `WebSocketServer::into_shared()` consumes the builder into a cheap, cloneable `SharedWebSocketServer` — the per-connection configuration is built once, each clone is an `Arc` clone, and the handle is `Send + Sync + 'static`. Then, for each accepted stream, classify it and route it: `is_websocket_upgrade(&stream)` reports whether the client is opening a WebSocket upgrade, `WebSocketServer::accept(stream, path)` performs the REPE handshake (validating `path`), and `SharedWebSocketServer::serve_connection(ws)` runs that connection's reader/writer loop. Connect/disconnect hooks and any attached `PeerRegistry` fire exactly as under `serve`.

```rust
use repe::{is_websocket_upgrade, WebSocketServer};

let shared = WebSocketServer::new(router).into_shared();
let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await?;
loop {
    let (stream, _addr) = listener.accept().await?;
    let shared = shared.clone();
    tokio::spawn(async move {
        match is_websocket_upgrade(&stream).await {
            Ok(true) => {
                if let Ok(ws) = WebSocketServer::accept(stream, "/repe").await {
                    let _ = shared.serve_connection(ws).await;
                }
            }
            Ok(false) => { let _ = serve_http(stream).await; }  // your own HTTP handler
            Err(err) => eprintln!("peek failed: {err}"),
        }
    });
}
```

`is_websocket_upgrade` is **non-destructive**: it peeks (`TcpStream::peek`) and leaves the request bytes in the socket buffer, because `WebSocketServer::accept` replays the handshake from the start of the stream. It is a sniff — it looks for an `Upgrade: websocket` header — and `accept` still does the authoritative RFC 6455 validation on the `true` branch, so a false positive fails the handshake rather than corrupting anything. A helper that *parsed* the request would consume the bytes and break `accept`; the peek-and-sniff keeps repe out of the business of serving HTTP while removing the trickiest part of the fork.

`examples/websocket_cohosting.rs` is a runnable end-to-end demo: it fills in both pieces the sketch above leaves open — the `is_websocket_upgrade` classifier and a minimal, dependency-free `serve_http` (a `/healthz` probe plus a small JSON `GET`) — and drives both forks against one port. Run it with `cargo run --example websocket_cohosting --features websocket`.

The built-in `serve` / `serve_listener` loop is itself implemented on top of `into_shared` + `accept` + `serve_connection`, so a co-hosted connection behaves identically to one accepted by the built-in loop.

### Draining a co-hosted server

Because a co-hosting embedder owns its accept loop, it cannot hand its listener to `serve_listener_with_graceful_drain`. To get the cancellation half on its own terms, create one `ShutdownToken`, serve each connection with `serve_connection_with_cancel(ws, &token)` instead of `serve_connection(ws)`, and `token.cancel()` on shutdown — every connection's in-flight off-reader handlers then observe [`ctx.is_cancelled()`](#cancellation-on-disconnect-and-shutdown) at once. Track the futures in your own `JoinSet` (as you already do for one-port hosting) and drain it with whatever deadline you choose.

```rust
use repe::{ShutdownToken, WebSocketServer};

let shared = WebSocketServer::new(router).into_shared();
let shutdown = ShutdownToken::new();
// ... in your accept loop, for each REPE upgrade:
let (shared, shutdown) = (shared.clone(), shutdown.clone());
conns.spawn(async move {
    if let Ok(ws) = WebSocketServer::accept(stream, "/repe").await {
        let _ = shared.serve_connection_with_cancel(ws, &shutdown).await;
    }
});
// ... on shutdown:
shutdown.cancel();           // wake every connection's in-flight handlers
// then drain your own JoinSet with a deadline, aborting stragglers.
```

Plain `serve_connection(ws)` still fires each connection's cancellation signal on **disconnect**; `serve_connection_with_cancel` additionally ties it to the shared token, so cancelling the token triggers all connections at once. The `ShutdownToken` hides its backing `tokio_util` cancellation type, so you are not coupled to that crate's version.
