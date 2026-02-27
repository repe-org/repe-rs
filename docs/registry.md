# Registry Support

`repe-rs` includes a dynamic `Registry` that can be mounted on a `Router` for JSON Pointer style access to values and callable endpoints.

## Semantics

For a request path resolved within a registry:

- Empty body: READ the value at the path.
- Non-empty body + function target: CALL the function with decoded params.
- Non-empty body + non-function target: WRITE the value at the path.

This matches the intended Glaze-style registry behavior.

## Quick Start

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
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Path Prefix

Mounting with a prefix strips that prefix from incoming paths before registry lookup.

Example:
- Router mount: `with_registry("/api/v1", registry)`
- Incoming request: `/api/v1/counter`
- Registry pointer resolved as: `/counter`

## Supported Body Formats

Registry request bodies are decoded as:

- `BodyFormat::Json` -> JSON value
- `BodyFormat::Beve` -> JSON value decoded from BEVE
- `BodyFormat::Utf8` -> JSON string value
- `BodyFormat::RawBinary` -> JSON array of byte integers

Unknown body formats are rejected with `InvalidBody`.

## Examples

- Server example: `cargo run --example registry_server`
- Local roundtrip example (no TCP): `cargo run --example registry_roundtrip`

## Client APIs

Registry reads require an empty body, and the client API now supports this directly:

- `Client::registry_read(path)` / `AsyncClient::registry_read(path)`
- `Client::registry_read_typed::<_, R>(path)` / `AsyncClient::registry_read_typed::<_, R>(path)`
- `Client::call_message(path)` / `AsyncClient::call_message(path)` for full `Message` access

WRITE/CALL operations keep the JSON helper shape:

- `Client::registry_write_json(path, body)` / async equivalent
- `Client::registry_call_json(path, body)` / async equivalent

### Sync Client Example

```rust
use repe::Client;
use serde_json::json;

let client = Client::connect("127.0.0.1:8082")?;

let counter = client.registry_read("/api/v1/counter")?;
println!("counter={counter}");

let _ = client.registry_write_json("/api/v1/counter", &json!(42))?;
let sum = client.registry_call_json("/api/v1/add", &json!({"a": 2, "b": 3}))?;
println!("sum={sum}");

#[derive(serde::Deserialize)]
struct Snapshot {
    counter: i64,
}
let _snapshot: Snapshot = client.registry_read_typed("/api/v1/state")?;

let raw_message = client.call_message("/api/v1/counter")?;
println!("raw response body format={}", raw_message.header.body_format);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Generic Formats

For non-standard query/body formats, use:

- `Client::call_with_formats(...)` / `AsyncClient::call_with_formats(...)`
- `Client::notify_with_formats(...)` / `AsyncClient::notify_with_formats(...)`

These APIs allow:
- custom `query_format` code
- optional raw body bytes (including true empty body)
- custom `body_format` code
