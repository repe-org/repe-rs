# Registry Support

`repe-rs` includes a dynamic `Registry` that can be mounted on a `Router` for JSON Pointer access to callable endpoints.

## Semantics

A registry is a flat table of functions keyed by canonical JSON Pointer. Resolving a path is one hash lookup on the whole pointer, and then the handler runs.

- A pointer names a **function**. There is no other kind of entry.
- The **body is the arguments**. An empty frame calls the function with `None`.
- What the function writes is the response.

There used to be a third thing here: a pointer could name a stored `serde_json::Value`, an empty body meant READ, and a non-empty body against a non-function meant WRITE. That went with the document model. A stateful endpoint is now a function that owns its state and decides for itself what a body means — which is also what lets it validate, reject, or apply rather than being assigned to blind.

## Quick Start

```rust
use repe::structs::RequestBody;
use repe::{ErrorCode, Registry, Router, Server};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Default)]
struct Operands { a: i64, b: i64 }
structio::object!(Operands { a, b });

#[derive(Default)]
struct Sum { result: i64 }
structio::object!(Sum { result });

let registry = Arc::new(Registry::new());

// A counter behind a function: a body sets it, no body reads it.
let counter = Arc::new(AtomicI64::new(0));
registry.register_function("/counter", move |params: Option<RequestBody<'_>>| {
    if let Some(body) = params {
        let next: i64 = body
            .read("/counter")
            .map_err(|e| (ErrorCode::InvalidBody, e.to_string()))?;
        counter.store(next, Ordering::SeqCst);
    }
    Ok(counter.load(Ordering::SeqCst))
})?;

registry.register_function("/add", |params: Option<RequestBody<'_>>| {
    let Some(body) = params else {
        return Err((ErrorCode::InvalidBody, "expected an object body".into()));
    };
    let operands: Operands = body
        .read("/add")
        .map_err(|e| (ErrorCode::InvalidBody, e.to_string()))?;
    Ok(Sum { result: operands.a + operands.b })
})?;

let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
let server = Server::new(router);
let listener = server.listen("127.0.0.1:8082")?;
server.serve(listener)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Migrating a registered value

Where the value has a type, declare it as a field and mount the struct — the derive publishes every field as an endpoint, which is what `register_value` was approximating:

```rust,ignore
// Before: registry.register_value("/counter", json!(0))?;
#[derive(Default, repe::RepeStruct)]
struct State { counter: i64 }
structio::object!(State { counter });

let (router, state) = router.with_struct("", State::default());
```

Where it genuinely has no type — a passthrough blob — it is a function that returns it.

## Path Prefix

Mounting with a prefix strips that prefix from incoming paths before registry lookup.

- Router mount: `with_registry("/api/v1", registry)`
- Incoming request: `/api/v1/counter`
- Registry pointer resolved as: `/counter`

## Body Formats

The registry does not decode a body. It hands the handler a `RequestBody` — the frame's bytes plus the format its header declared — and the handler reads them as whatever type it expects. A request no endpoint claims is never parsed at all.

All four known formats reach a handler, `RawBinary` included: a handler that means to treat the body as bytes calls `RequestBody::bytes` and never parses it. A format code this build does not recognize is rejected with `InvalidBody`, because handing a handler bytes under a format it cannot name is worse than refusing.

The response follows the request: a BEVE call is answered in BEVE, a JSON call in JSON. Nothing is transcoded — the handler writes into the response buffer in the negotiated format.

## Examples

- Server example: `cargo run --example registry_server`
- Local roundtrip example (no TCP): `cargo run --example registry_roundtrip`

## Client APIs

- `Client::call_typed_json(path, body)` / `AsyncClient::call_typed_json(path, body)` — call with a JSON body
- `Client::call_typed_beve(path, body)` / async equivalent — the same, in BEVE
- `Client::registry_read_typed::<_, R>(path)` / async equivalent — call with an empty body
- `Client::call_message(path)` / async equivalent — full `Message` access

Every one of them names the type it expects back. There is no untyped call: with no document model a response has to be decoded into something, and naming that something turns a wrong answer into a decode error rather than a `None` found three lines later.

### Sync Client Example

```rust
use repe::Client;

#[derive(Default)]
struct Operands { a: i64, b: i64 }
structio::object!(Operands { a, b });

#[derive(Default)]
struct Sum { result: i64 }
structio::object!(Sum { result });

let client = Client::connect("127.0.0.1:8082")?;

let counter: i64 = client.registry_read_typed("/api/v1/counter")?;
println!("counter={counter}");

let _: i64 = client.call_typed_json("/api/v1/counter", &42i64)?;
let sum: Sum = client.call_typed_json("/api/v1/add", &Operands { a: 2, b: 3 })?;
println!("sum={}", sum.result);

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
- optional raw body bytes (including a true empty body)
- custom `body_format` code
