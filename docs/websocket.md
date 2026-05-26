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

`PeerHandle::send_notify` returning `Ok(())` means the message was queued onto the outbound channel, not that it was written to the wire. During concurrent disconnect, a late-arriving send may queue successfully but be dropped when the writer task exits and the bounded receiver is released. This is intrinsic to async shutdown; treat `Ok` as best-effort delivery, not as confirmation.

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

Middleware does not need to be aware of `CallContext` to forward it. `Next::run(req)` threads whatever the upstream caller attached, so existing middleware composes transparently with new context-aware handlers. Handlers registered via `with_json` / `with_typed` (the legacy non-`_ctx` variants) keep working unchanged; they simply do not receive the context.

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

Both the saturation rejection and the caught panic surface as `ErrorCode::ApplicationErrorBase`: REPE defines no protocol-level "unavailable" or "internal error" code today, so a client cannot tell these apart from an ordinary application error by code alone. Treat this as a known protocol gap.

`with_offreader_limit` bounds **per-connection fairness, not the global blocking pool.** `spawn_blocking` draws from tokio's process-wide blocking pool (512 threads by default), and a producer parked in `wait_for_credit` holds its thread for the whole transfer. Embedders expecting many concurrent streaming connections should raise the runtime's `max_blocking_threads` or use a dedicated runtime. The idle watchdog (`spawn_watchdog`, see [streaming.md](streaming.md)) reclaims both the blocking-pool thread and the permit from a transfer that goes silent.

### One producer per transfer

Removing per-connection serialization also removes an implicit safety net. Inline, a second `/begin` for the same transfer id cannot start until the first returns; off-reader, two `begin` frames spawn two producers against one `TransferControl`, violating its single-producer assumption and racing the registry entry (the second `begin` overwrites it while both producers run). The concurrency cap bounds the *count* of off-reader tasks, not duplicate registration, so a `begin`-style handler **must reject (or de-duplicate) a transfer id that is already registered** — check `TransferRegistry::get` before `register` and return an application error if it is already present.
