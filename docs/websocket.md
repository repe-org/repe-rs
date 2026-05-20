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

### Compatibility

The push path is strictly opt-in. Existing `WebSocketServer::new(router).serve(addr, path)` callers see no behavior change, no new error variants, and no protocol changes. Notify frames are the same shape they have always been (`Message::builder().notify(true)`).
