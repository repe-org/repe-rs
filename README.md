# repe-rs

Rust implementation of the REPE RPC protocol with JSON and BEVE body support.

- Spec reference: <https://github.com/repe-org/REPE>
- Crate: [repe on crates.io](https://crates.io/crates/repe)

This crate provides:

- REPE header and message types with correct little-endian encoding
- Message encode/decode to/from bytes and I/O helpers for streams
- JSON body helpers using `serde`/`serde_json`
- BEVE binary body helpers via the [`beve`](https://crates.io/crates/beve) crate
- Dynamic registry routing with JSON Pointer semantics
- Error codes and formats aligned with the spec
- Optional native WebSocket transport (`websocket` feature)
- Optional browser WebSocket transport for wasm (`websocket-wasm` feature)

Installation

Add to your `Cargo.toml`:

```
[dependencies]
repe = "1.1"
```

> Tip: run `cargo add repe` to automatically pull the latest compatible release.

Quick start

```rust
use repe::{Message, QueryFormat, BodyFormat};

// Build a JSON message
let msg = Message::builder()
    .id(42)
    .query_str("/status")
    .query_format(QueryFormat::JsonPointer)
    .body_json(&serde_json::json!({"ping": true}))?
    .build();

// Serialize to bytes and back
let bytes = msg.to_vec();
let parsed = repe::Message::from_slice(&bytes)?;

assert_eq!(parsed.header.id, 42);
assert_eq!(parsed.header.body_format, BodyFormat::Json as u16);
let val: serde_json::Value = parsed.json_body()?;
assert_eq!(val["ping"], true);
```

```rust
use repe::{Message, QueryFormat, BodyFormat};

// Build a BEVE message
let msg = Message::builder()
    .id(43)
    .query_str("/status")
    .query_format(QueryFormat::JsonPointer)
    .body_beve(&serde_json::json!({"ping": true}))?
    .build();

let bytes = msg.to_vec();
let parsed = repe::Message::from_slice(&bytes)?;

assert_eq!(parsed.header.body_format, BodyFormat::Beve as u16);
let val: serde_json::Value = parsed.beve_body()?;
assert_eq!(val["ping"], true);
```

Streaming and zero-copy I/O

For large bodies (e.g. multi-MiB binary chunks), `Message::to_vec` /
`Message::from_slice` add a full-body memcpy you may not want. The streaming
APIs below let the body flow directly between the wire and a user-supplied
encoder/decoder.

```rust
use repe::{write_message_streaming, BodyFormat, Header, QueryFormat};
use std::io::Write;

// Send: BEVE-encode a body straight into the writer with no intermediate Vec.
let mut header = Header::new();
header.id = 99;
header.query_format = QueryFormat::JsonPointer as u16;
header.body_format = BodyFormat::Beve as u16;
let body_len = beve_chunk_size(&chunk); // computed up front

write_message_streaming(&mut socket, header, b"/collect/file_chunk", body_len, |w| {
    beve::to_writer_streaming(w, &chunk).map_err(std::io::Error::other)
})?;
```

```rust
use repe::MessageView;

// Receive: borrow into the WS frame buffer instead of copying out of it.
let view = MessageView::from_slice(&frame_bytes)?;
let path = view.query_str()?;
// view.body is &[u8] pointing inside frame_bytes; pair with serde_bytes::Bytes<'a>
// on a Deserialize struct to keep the chunk payload borrowed end-to-end.
```

`Message::write_to` and `Message::serialized_len` are the owned-`Message`
counterparts: emit a `Message` to a `Write` without going through `to_vec`,
or query its wire size in `O(1)`.

Peer-aware handlers

When a handler needs to push more than one message back to the calling
client (e.g. server-pushed file chunks after a single `/run_collection`
call), it needs a typed handle to that specific connection. `PeerSink` /
`PeerHandle` / `CallContext` provide that handle; `Registry::dispatch_with_ctx`
threads it through to handlers.

Repe-rs's built-in TCP/WebSocket servers do not yet construct `PeerHandle`s
themselves. Embedders that want peer routing wire their own `PeerSink`
(typically a bounded channel drained by a writer task) against their
server's outbound side.

```rust
use repe::{
    CallContext, NotifyBody, PeerHandle, PeerId, PeerSendError, PeerSink,
    Registry, WithContext,
};
use serde_json::{json, Value};
use std::sync::Arc;

// Implement PeerSink against your server's outbound mechanism.
struct OutboundChannel(/* tx: mpsc::Sender<...> */);
impl PeerSink for OutboundChannel {
    fn send_notify(&self, _method: &str, _body: NotifyBody) -> Result<(), PeerSendError> {
        // push a notify Message onto the peer's outbound queue.
        Ok(())
    }
}

// Register a context-aware handler. WithContext is the marker that opts
// into the &CallContext parameter; plain Fn(Option<Value>) -> Result<...>
// closures keep working unchanged.
let registry = Registry::new();
registry.register_function("/run", WithContext(|ctx: &CallContext, _params| {
    if let Some(peer) = ctx.peer() {
        peer.send_notify("/progress", NotifyBody::Json(b"{\"step\":1}".to_vec())).ok();
    }
    Ok::<_, (repe::ErrorCode, String)>(json!({"status": "ok"}))
})).unwrap();

// At dispatch time, build a CallContext for the calling peer and invoke
// dispatch_with_ctx. Plain dispatch() supplies CallContext::detached.
let peer = PeerHandle::new(PeerId(1), Arc::new(OutboundChannel(/* ... */)));
let ctx = CallContext::new("/run", &peer);
let _ = registry.dispatch_with_ctx("/run", Some(json!({})), &ctx);
```

Examples

- `examples/json_roundtrip.rs` – construct/serialize/parse a JSON message.
- `examples/server.rs` – run a JSON-pointer server (TCP).
- `examples/client.rs` – call the server routes.
- `examples/registry_server.rs` – serve a dynamic registry under a path prefix.
- `examples/registry_roundtrip.rs` – local registry READ/WRITE/CALL roundtrip.
- `examples/async_server.rs` – tokio-based async server.
- `examples/async_client.rs` – tokio-based async client.

Notes

- BEVE helpers encode/decode via serde, so your existing request/response types continue to work.
- The header `reserved` field is validated and must be zero.
- The header length is validated to be `48 + query_length + body_length`.

Feature flags

- `websocket` - native `WebSocketClient`, `WebSocketServer`, and `proxy_connection`
- `websocket-wasm` - browser `WasmClient` on `wasm32-unknown-unknown`
- `fleet-udp` - UDP fanout support via `UniUdpFleet`
- `parking-lot` - `Lockable` support for `parking_lot::Mutex` / `RwLock`

WebSocket transport

- Each REPE message maps to exactly one WebSocket binary message.
- WebSocket decoding enforces exact message length within each bounded binary frame.

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

JSON Pointer Routing and Typed Handlers

- Router keys are JSON Pointer paths (e.g., `/ping`, `/echo`, `/status`). Raw-binary queries are rejected with an explicit `Invalid query` error.
- Add JSON Value handlers with `.with("/path", |v: serde_json::Value| -> Result<Value, (ErrorCode,String)>)`.
- Add typed handlers with `.with_typed("/path", |req: T| -> Result<R, (ErrorCode,String)>)` where `T: Deserialize`, `R: Serialize`.
  - Typed handlers auto-deserialize JSON, UTF-8, or BEVE bodies into `T`. Responses default to JSON.
  - Wrap the return value with `TypedResponse::beve(...)` / `TypedResponse::utf8(...)` / etc. to pick a different response [`BodyFormat`].
  - If a body arrives with an unsupported format for typed handlers, the server returns `Invalid body`.
- Attach pre-request middleware with `.with_middleware(|req, next| { /* auth/logging */ next.run(req) })` to centralize auth, validation, or tracing without hand-wrapping handlers.

- Implement the pluggable trait `JsonTypedHandler` to attach methods from a service type:

```rust
use repe::{Router, JsonTypedHandler, ErrorCode};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Input { name: String }
#[derive(Serialize)]
struct Output { greeting: String }

struct Greeter;
impl JsonTypedHandler for Greeter {
    type In = Input;
    type Out = Output;
    fn call(&self, input: Self::In) -> Result<Self::Out, (ErrorCode, String)> {
        Ok(Output { greeting: format!("Hello, {}!", input.name) })
    }
}

let router = Router::new().with_handler("/greet", Greeter);
```

Async (tokio)

- Use `AsyncServer` and `AsyncClient` for asynchronous operation with tokio.
- See `examples/async_server.rs` and `examples/async_client.rs`.

Fleet (Multi-Node Control)

- Use `Fleet` / `AsyncFleet` to manage multiple TCP REPE servers as one logical unit.
- Use `UniUdpFleet` for fire-and-forget UDP fanout (enable with `--features fleet-udp`).
- TCP retry policy retries transport/I/O failures only; application-level server errors are returned without retry.
- See [docs/fleet.md](docs/fleet.md) for complete API details.

```rust
use repe::{Fleet, FleetOptions, NodeConfig, RetryPolicy};
use serde_json::json;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fleet = Fleet::with_options(
        vec![
            NodeConfig::new("127.0.0.1", 8081)?
                .with_name("node-1")?
                .with_tags(["compute"]),
            NodeConfig::new("127.0.0.1", 8082)?
                .with_name("node-2")?
                .with_tags(["compute", "primary"]),
        ],
        FleetOptions {
            default_timeout: Duration::from_secs(2),
            retry_policy: RetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(100),
            },
        },
    )?;

    let _ = fleet.connect_all();
    let status = fleet.broadcast_json("/status", None, &[] as &[&str]);
    let total = fleet.map_reduce_json(
        "/compute",
        Some(&json!({"value": 10})),
        &["compute"],
        |results| {
            results
                .into_iter()
                .filter_map(|r| r.value.and_then(|v| v["result"].as_i64()))
                .sum::<i64>()
        },
    );
    println!("total={total}, nodes={}", status.len());
    Ok(())
}
```

```rust
use repe::{AsyncFleet, FleetOptions, NodeConfig, RetryPolicy};
use serde_json::json;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fleet = AsyncFleet::with_options(
        vec![NodeConfig::new("127.0.0.1", 8081)?.with_name("node-1")?],
        FleetOptions {
            default_timeout: Duration::from_secs(2),
            retry_policy: RetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(100),
            },
        },
    )?;

    let _ = fleet.connect_all().await;
    let res = fleet
        .call_json("node-1", "/status", Some(&json!({})))
        .await?;
    assert!(res.succeeded());
    Ok(())
}
```

```rust
use repe::{UniUdpFleet, UniUdpNodeConfig};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fleet = UniUdpFleet::new(vec![
        UniUdpNodeConfig::new("127.0.0.1", 5001)?
            .with_name("edge-a")?
            .with_tags(["edge"]),
        UniUdpNodeConfig::new("127.0.0.1", 5002)?
            .with_name("edge-b")?
            .with_tags(["edge"]),
    ])?;

    let sent = fleet.send_notify("/heartbeat", Some(&json!({"source": "controller"})), &["edge"]);
    assert!(sent.values().all(|result| result.succeeded()));
    Ok(())
}
```

Server

- Build a router and serve TCP:

```rust
use repe::{Router, Server};
use serde_json::json;
use std::time::Duration;

let router = Router::new()
    .with_middleware(|req, next| {
        if let Ok(path) = req.query_str() {
            println!("incoming request for {path}");
        }
        next.run(req)
    })
    .with("/ping", |_v| Ok(json!({"pong": true})))
    .with("/echo", |v| Ok(json!({"echo": v})))
    .with("/status", |_v| Ok(json!({"status": "ok"})));

let server = Server::new(router)
    .read_timeout(Some(Duration::from_secs(120)))
    .write_timeout(Some(Duration::from_secs(120)));
let listener = server.listen("127.0.0.1:8081")?;
server.serve(listener)?;
```

- Register a struct and expose its fields/methods through JSON pointer paths:

```rust
use repe::Router;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods(
    greet(&self) -> String,
    set_status(&mut self, new_status: String) -> (),
    reset_metrics(&mut self) -> ()
))]
struct Device {
    id: String,
    status: String,
    #[repe(nested)]
    metrics: Metrics,
}

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
struct Metrics {
    temperature: f64,
    humidity: f64,
}

impl Device {
    fn greet(&self) -> String {
        format!("device {} reporting {}", self.id, self.status)
    }

    fn set_status(&mut self, new_status: String) {
        self.status = new_status;
    }

    fn reset_metrics(&mut self) {
        self.metrics = Metrics::default();
    }
}

let mut router = Router::new();
let device_handle = router.register_struct("/device", Device::default());

// Update initial state before serving.
{
    let mut device = device_handle.lock().unwrap();
    device.id = "sensor-42".into();
    device.status = "online".into();
    device.metrics.temperature = 21.5;
    device.metrics.humidity = 0.55;
}

// Example calls (JSON Pointer paths):
// - `/device/greet` -> "device sensor-42 reporting online"
// - `/device/status` with body "offline" writes the field and returns null
// - `/device/metrics/temperature` reads the nested value (21.5)
// - `/device/reset_metrics` zeroes out the metrics

Router handles `Arc<L>` for any lock implementing `repe::Lockable<T>`, so you can swap in
`tokio::sync::Mutex`/`RwLock` (via their `blocking_*` APIs) or enable the optional
`parking-lot` feature to use `parking_lot::Mutex`/`RwLock` without extra wrapper types.
```

Registry (Dynamic Routing)

- Mount a `Registry` on any path prefix with `Router::with_registry("/api/v1", registry)`.
- Registry request semantics:
  - Empty body => READ value.
  - Non-empty body + function target => CALL function.
  - Non-empty body + non-function target => WRITE value.
- See [docs/registry.md](docs/registry.md) for full behavior, format decoding details, and client examples.

```rust
use repe::{ErrorCode, Registry, Router, Server};
use serde_json::{Value, json};
use std::sync::Arc;

let registry = Arc::new(Registry::new());
registry.register_value("/counter", json!(0))?;
registry.register_function("/add", |params| {
    let Some(Value::Object(map)) = params else {
        return Err((ErrorCode::InvalidBody, "expected object body".into()));
    };
    let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
    let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
    Ok(json!({"result": a + b}))
})?;

let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
let server = Server::new(router);
let listener = server.listen("127.0.0.1:8082")?;
server.serve(listener)?;
```

Client

- Connect and call JSON-pointer routes with JSON bodies:

```rust
use repe::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize)]
struct AddReq {
    a: i64,
    b: i64,
}

#[derive(Deserialize)]
struct AddResp {
    sum: i64,
}

let client = Client::connect("127.0.0.1:8081")?;
let pong = client.call_json("/ping", &json!({}))?;
assert_eq!(pong["pong"], true);

let typed: AddResp = client.call_typed_json("/add", &AddReq { a: 2, b: 3 })?;
assert_eq!(typed.sum, 5);

client.notify_typed_json("/jobs/refresh", &AddReq { a: 0, b: 0 })?;
client.notify_typed_beve("/jobs/refresh_beve", &AddReq { a: 1, b: 2 })?;
```

Typed helpers work for BEVE payloads too:

```rust
let beve_sum: AddResp = client.call_typed_beve("/add", &AddReq { a: 4, b: 5 })?;
assert_eq!(beve_sum.sum, 9);
```

Registry-oriented client helpers:

- Empty-body READs: `registry_read("/api/v1/counter")` or `call_message(...)`
- Typed READs: `registry_read_typed::<_, MyType>(...)`
- JSON WRITE/CALL helpers: `registry_write_json(...)` and `registry_call_json(...)`
- Custom wire formats: `call_with_formats(...)` and `notify_with_formats(...)`

Multiplexed calls, timeouts, and batching

- `Client` and `AsyncClient` can run multiple in-flight requests on one connection.
- Clone the client handle and issue calls concurrently; responses are matched by request `id` even if the server replies out of order.
- Use per-call timeout helpers:
  - `call_json_with_timeout`
  - `call_typed_json_with_timeout`
  - `call_typed_beve_with_timeout`
- Use batch helpers for JSON calls:
  - `batch_json(Vec<(String, Value)>)`
  - `batch_json_with_timeout(Vec<(String, Value)>, Duration)`

Notify semantics

- If a request is marked `notify = true`, the server will process the handler but will not send a response.
- In the protocol, the `id` still increments client-side, but no matching response should be expected.
- The `Client::notify_json` and `AsyncClient::notify_json` helpers set the flag accordingly.
- `Client::notify_typed_json` / `Client::notify_typed_beve` and their async counterparts send typed payloads without waiting for a response, mirroring the call helpers.

Error handling

- Errors are returned “in-band” as responses with `ec != Ok` and a UTF‑8 body describing the error.
- Common mappings:
  - Header parse/validation issues → `InvalidHeader` or `ParseError`.
  - JSON deserialization/serialization issues → `ParseError`.
  - Missing routes → `MethodNotFound` with the requested path in the message.
  - Application failures from handlers → return `(ErrorCode, String)` to control both fields.
- Clients surface mismatched protocol versions as `RepeError::VersionMismatch`.
- Responses with unknown request IDs are logged and dropped by default (including late responses for timed-out requests).
- For bounded latency, prefer `call_*_with_timeout` APIs so a dropped response ID cannot leave a call waiting indefinitely.

Async usage (minimal end‑to‑end)

```rust
use repe::{Router, AsyncServer, AsyncClient};
use serde::{Deserialize, Serialize};
use serde_json::json;

# #[tokio::main]
# async fn main() -> std::io::Result<()> {
let router = Router::new().with_json("/ping", |_v| Ok(json!({"pong": true})));
let listener = AsyncServer::listen(("127.0.0.1", 0)).await?;
let addr = listener.local_addr()?;
tokio::spawn(async move { let _ = AsyncServer::new(router).serve(listener).await; });

#[derive(Serialize)]
struct AddReq {
    a: i64,
    b: i64,
}

#[derive(Deserialize)]
struct AddResp {
    sum: i64,
}

let client = AsyncClient::connect(addr).await.unwrap();
let pong = client.call_json("/ping", &json!({})).await.unwrap();
assert_eq!(pong["pong"], true);

let typed: AddResp = client
    .call_typed_json("/add", &AddReq { a: 8, b: 1 })
    .await
    .unwrap();
assert_eq!(typed.sum, 9);

let beve: AddResp = client
    .call_typed_beve("/add", &AddReq { a: 5, b: 6 })
    .await
    .unwrap();
assert_eq!(beve.sum, 11);
# Ok(()) }
```

Running the examples

```
cargo run --example server
cargo run --example client
cargo run --example registry_server
cargo run --example registry_roundtrip
cargo run --example async_server
cargo run --example async_client
```

Design overview

- Fixed header size is `48` bytes (`HEADER_SIZE`) followed by `query` and `body` payloads.
- All numeric fields are little‑endian. Header `length` equals `48 + query_length + body_length` and is validated.
- `query_format`/`body_format` use the enums in `repe::constants`.
- Suggested query format is JSON Pointer (`/path/to/resource`).

JSON Pointer helpers

- `parse_json_pointer` splits a pointer into unescaped tokens.
- `eval_json_pointer` indexes into a `serde_json::Value` using array indices or object keys.

Testing

- Run `cargo test` to execute unit and integration tests. The crate denies warnings and includes async tests; you’ll need a recent tokio.
- Example integration tests cover sync/async server + client calls, unknown routes, handler error mapping, ID mismatches, and timeouts.

License

- MIT, see `LICENSE`.
