# repe-rs

Rust implementation of the REPE RPC protocol, focused on JSON as the body format.

- Spec reference: `reference/REPE/README.md`
- Julia example: `reference/REPE.jl`

This crate provides:

- REPE header and message types with correct little-endian encoding
- Message encode/decode to/from bytes and I/O helpers for streams
- JSON body helpers using `serde`/`serde_json`
- Error codes and formats aligned with the spec

Installation

Add to your `Cargo.toml`:

```
[dependencies]
repe = { path = "." } # or use a version once published
```

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

Examples

- `examples/json_roundtrip.rs` – construct/serialize/parse a JSON message.
- `examples/server.rs` – run a JSON-pointer server (TCP).
- `examples/client.rs` – call the server routes.
- `examples/async_server.rs` – tokio-based async server.
- `examples/async_client.rs` – tokio-based async client.

Notes

- Only JSON, UTF-8 (for error messages), and raw bytes helpers are implemented. BEVE is intentionally not implemented.
- The header `reserved` field is currently ignored (per spec receivers must not reject non-zero values).
- The header length is validated to be `48 + query_length + body_length`.

JSON Pointer Routing and Typed Handlers

- Router keys are JSON Pointer paths (e.g., `/ping`, `/echo`, `/status`). Raw-binary queries are rejected with an explicit `Invalid query` error.
- Add JSON Value handlers with `.with("/path", |v: serde_json::Value| -> Result<Value, (ErrorCode,String)>)`.
- Add typed handlers with `.with_typed("/path", |req: T| -> Result<R, (ErrorCode,String)>)` where `T: Deserialize`, `R: Serialize`.
  - Typed handlers auto-deserialize JSON bodies into `T` and serialize `R` to JSON responses.
  - If a non-JSON body is sent to a typed/JSON handler, the server returns `Invalid body`.

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

Server

- Build a router and serve TCP:

```rust
use repe::{Router, Server};
use serde_json::json;
use std::time::Duration;

let router = Router::new()
    .with("/ping", |_v| Ok(json!({"pong": true})))
    .with("/echo", |v| Ok(json!({"echo": v})))
    .with("/status", |_v| Ok(json!({"status": "ok"})));

let server = Server::new(router)
    .read_timeout(Some(Duration::from_secs(120)))
    .write_timeout(Some(Duration::from_secs(120)));
let listener = server.listen("127.0.0.1:8081")?;
server.serve(listener)?;
```

Client

- Connect and call JSON-pointer routes with JSON bodies:

```rust
use repe::Client;
use serde_json::json;

let mut client = Client::connect("127.0.0.1:8081")?;
let pong = client.call_json("/ping", &json!({}))?;
assert_eq!(pong["pong"], true);
```

Notify semantics

- If a request is marked `notify = true`, the server will process the handler but will not send a response.
- In the protocol, the `id` still increments client-side, but no matching response should be expected.
- The `Client::notify_json` and `AsyncClient::notify_json` helpers set the flag accordingly.

Error handling

- Errors are returned “in-band” as responses with `ec != Ok` and a UTF‑8 body describing the error.
- Common mappings:
  - Header parse/validation issues → `InvalidHeader` or `ParseError`.
  - JSON deserialization/serialization issues → `ParseError`.
  - Missing routes → `MethodNotFound` with the requested path in the message.
  - Application failures from handlers → return `(ErrorCode, String)` to control both fields.
- Clients also surface mismatched response IDs as `RepeError::ResponseIdMismatch` and mismatched protocol versions as `RepeError::VersionMismatch`.

Async usage (minimal end‑to‑end)

```rust
use repe::{Router, AsyncServer, AsyncClient};
use serde_json::json;

# #[tokio::main]
# async fn main() -> std::io::Result<()> {
let router = Router::new().with_json("/ping", |_v| Ok(json!({"pong": true})));
let listener = AsyncServer::listen(("127.0.0.1", 0)).await?;
let addr = listener.local_addr()?;
tokio::spawn(async move { let _ = AsyncServer::new(router).serve(listener).await; });

let mut client = AsyncClient::connect(addr).await.unwrap();
let pong = client.call_json("/ping", &json!({})).await.unwrap();
assert_eq!(pong["pong"], true);
# Ok(()) }
```

Running the examples

```
cargo run --example server
cargo run --example client
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
