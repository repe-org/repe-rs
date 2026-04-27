# Client APIs

`Client` is the synchronous TCP client; `AsyncClient` is the tokio equivalent. Both speak the same protocol and expose the same set of helpers.

## Connecting and Calling

```rust
use repe::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize)]
struct AddReq { a: i64, b: i64 }

#[derive(Deserialize)]
struct AddResp { sum: i64 }

let client = Client::connect("127.0.0.1:8081")?;

let pong = client.call_json("/ping", &json!({}))?;
assert_eq!(pong["pong"], true);

let typed: AddResp = client.call_typed_json("/add", &AddReq { a: 2, b: 3 })?;
assert_eq!(typed.sum, 5);

client.notify_typed_json("/jobs/refresh", &AddReq { a: 0, b: 0 })?;
client.notify_typed_beve("/jobs/refresh_beve", &AddReq { a: 1, b: 2 })?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

BEVE typed helpers work the same way:

```rust
let beve_sum: AddResp = client.call_typed_beve("/add", &AddReq { a: 4, b: 5 })?;
assert_eq!(beve_sum.sum, 9);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Async Client

```rust
use repe::{AsyncClient, AsyncServer, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

# #[tokio::main]
# async fn main() -> std::io::Result<()> {
let router = Router::new().with("/ping", |_v| Ok(json!({"pong": true})));
let listener = AsyncServer::listen(("127.0.0.1", 0)).await?;
let addr = listener.local_addr()?;
tokio::spawn(async move { let _ = AsyncServer::new(router).serve(listener).await; });

#[derive(Serialize)]
struct AddReq { a: i64, b: i64 }
#[derive(Deserialize)]
struct AddResp { sum: i64 }

let client = AsyncClient::connect(addr).await.unwrap();
let pong = client.call_json("/ping", &json!({})).await.unwrap();
assert_eq!(pong["pong"], true);

let typed: AddResp = client
    .call_typed_json("/add", &AddReq { a: 8, b: 1 })
    .await
    .unwrap();
assert_eq!(typed.sum, 9);
# Ok(()) }
```

## Registry Helpers

For servers that mount a `Registry` (see [registry.md](registry.md)):

- Empty-body READs: `registry_read("/api/v1/counter")` or `call_message(...)`
- Typed READs: `registry_read_typed::<_, MyType>(...)`
- JSON WRITE/CALL: `registry_write_json(...)` and `registry_call_json(...)`
- Custom wire formats: `call_with_formats(...)` and `notify_with_formats(...)`

## Multiplexing, Timeouts, and Batching

Both `Client` and `AsyncClient` can run multiple in-flight requests on a single connection. Clone the client handle and issue calls concurrently; responses are matched by request `id` even if the server replies out of order.

Per-call timeout helpers:

- `call_json_with_timeout`
- `call_typed_json_with_timeout`
- `call_typed_beve_with_timeout`

Batch helpers for JSON calls:

- `batch_json(Vec<(String, Value)>)`
- `batch_json_with_timeout(Vec<(String, Value)>, Duration)`

## Notify Semantics

A request marked `notify = true` is processed by the server but produces no response. The client `id` still increments, but no matching response is expected.

- `Client::notify_json` / `AsyncClient::notify_json` set the flag for `serde_json::Value` bodies.
- `Client::notify_typed_json` / `notify_typed_beve` and their async counterparts send typed payloads without waiting for a response, mirroring the call helpers.

## Error Handling

Errors are returned in-band as responses with `ec != Ok` and a UTF-8 body describing the error. Common mappings:

- Header parse/validation issues: `InvalidHeader` or `ParseError`.
- JSON serialization or deserialization issues: `ParseError`.
- Missing routes: `MethodNotFound` with the requested path in the message.
- Application failures from handlers: return `(ErrorCode, String)` to control both fields.

Clients surface mismatched protocol versions as `RepeError::VersionMismatch`. Responses with unknown request IDs are logged and dropped by default, including late responses for timed-out requests. For bounded latency, prefer `*_with_timeout` APIs so a dropped response ID cannot leave a call waiting indefinitely.
