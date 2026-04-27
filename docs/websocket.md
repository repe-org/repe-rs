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
