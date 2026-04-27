# Server, Routers, and Handlers

`Router` maps JSON Pointer paths to handler closures or typed services. `Server` and `AsyncServer` accept TCP connections and dispatch them through a router. `WebSocketServer` (behind the `websocket` feature) does the same over WebSocket.

## Router

Add JSON `Value` handlers with `.with(path, fn)` and typed handlers with `.with_typed(path, fn)`. Typed handlers auto-deserialize JSON, UTF-8, or BEVE bodies into `T` and default to JSON responses; wrap the return with `TypedResponse::beve(...)` / `TypedResponse::utf8(...)` to pick a different response `BodyFormat`. Bodies in unsupported formats are rejected with `Invalid body`.

Pre-request middleware runs before the handler and can centralize auth, validation, or tracing.

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
# Ok::<(), Box<dyn std::error::Error>>(())
```

Router keys must be JSON Pointer paths (e.g. `/ping`, `/echo`). Raw-binary queries are rejected with `Invalid query`. Missing routes return `MethodNotFound` with the requested path.

## Typed Handlers via `JsonTypedHandler`

Implement the `JsonTypedHandler` trait to attach a service type's methods to a router:

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

## Registering a Struct

`register_struct` exposes a struct's fields and methods through JSON Pointer paths automatically. Annotate methods to publish with `#[repe(methods(...))]` and mark nested struct fields with `#[repe(nested)]`.

```rust
use repe::Router;
use serde::{Deserialize, Serialize};

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
    fn greet(&self) -> String { format!("device {} reporting {}", self.id, self.status) }
    fn set_status(&mut self, new_status: String) { self.status = new_status; }
    fn reset_metrics(&mut self) { self.metrics = Metrics::default(); }
}

let mut router = Router::new();
let device_handle = router.register_struct("/device", Device::default());

{
    let mut device = device_handle.lock().unwrap();
    device.id = "sensor-42".into();
    device.status = "online".into();
    device.metrics.temperature = 21.5;
    device.metrics.humidity = 0.55;
}
```

The resulting paths:

- `/device/greet` returns `"device sensor-42 reporting online"`.
- `/device/status` with body `"offline"` writes the field and returns null.
- `/device/metrics/temperature` reads the nested value `21.5`.
- `/device/reset_metrics` zeroes out the metrics.

`Router` accepts `Arc<L>` for any lock implementing `repe::Lockable<T>`, so you can swap in `tokio::sync::Mutex` / `RwLock` (via their `blocking_*` APIs) or enable the optional `parking-lot` feature to use `parking_lot::Mutex` / `RwLock` without extra wrapper types.

## Async Server

`AsyncServer` mirrors `Server` and runs on tokio. See `examples/async_server.rs`.

```rust
use repe::{AsyncServer, Router};
use serde_json::json;

# async fn run() -> std::io::Result<()> {
let router = Router::new().with("/ping", |_v| Ok(json!({"pong": true})));
let listener = AsyncServer::listen(("127.0.0.1", 0)).await?;
tokio::spawn(async move { let _ = AsyncServer::new(router).serve(listener).await; });
# Ok(()) }
```

## Peer-Aware Handlers

Handlers that need to push more than one message back to the calling client (e.g. server-pushed file chunks after a single `/run_collection` call) need a typed handle to that connection. `PeerSink` / `PeerHandle` / `CallContext` provide that handle, and `Registry::dispatch_with_ctx` threads it through to the handler.

The built-in TCP and WebSocket servers do not yet construct `PeerHandle`s themselves. Embedders that want peer routing wire their own `PeerSink` (typically a bounded channel drained by a writer task) against their server's outbound side.

```rust
use repe::{
    CallContext, NotifyBody, PeerHandle, PeerId, PeerSendError, PeerSink,
    Registry, WithContext,
};
use serde_json::{json, Value};
use std::sync::Arc;

struct OutboundChannel(/* tx: mpsc::Sender<...> */);
impl PeerSink for OutboundChannel {
    fn send_notify(&self, _method: &str, _body: NotifyBody) -> Result<(), PeerSendError> {
        // push a notify Message onto the peer's outbound queue.
        Ok(())
    }
}

let registry = Registry::new();
registry.register_function("/run", WithContext(|ctx: &CallContext, _params| {
    if let Some(peer) = ctx.peer() {
        peer.send_notify("/progress", NotifyBody::Json(b"{\"step\":1}".to_vec())).ok();
    }
    Ok::<_, (repe::ErrorCode, String)>(json!({"status": "ok"}))
})).unwrap();

let peer = PeerHandle::new(PeerId(1), Arc::new(OutboundChannel(/* ... */)));
let ctx = CallContext::new("/run", &peer);
let _ = registry.dispatch_with_ctx("/run", Some(json!({})), &ctx);
```

`WithContext` is the marker that opts a closure into the `&CallContext` parameter. Plain `Fn(Option<Value>) -> Result<...>` handlers keep working unchanged: `Registry::dispatch` is a thin wrapper that supplies a `CallContext::detached` context.
