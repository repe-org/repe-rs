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

### Routes that do not exist yet

Every registrar runs before `Server::serve`, so a path discovered at run time has nowhere to go. `Router::with_fallback(handler)` is where it goes: a handler invoked on a miss, resolved **last** — after the fixed routes, the mounted registries, and the mounted structs — so a static route is never slowed down by it, and wrapped in the middleware pipeline like any other route.

```rust
let router = Router::new()
    .with("/ping", |_v| Ok(json!({"pong": true})))
    .with_fallback(Arc::new(dynamic_table));
```

It is the natural mount for anything whose table is built while the server runs: a [plugin](plugins.md#mounting-a-plugin-on-a-router) loaded on demand, a proxy to another node, a registry mounted after startup. There was no way to do this before, not even an awkward one — middleware is attached per route, so on a miss no pipeline exists and none of it runs.

The handler owns every miss it is given: nothing else will answer, so one that does not claim the request has to frame `MethodNotFound` itself — which is also what lets it decline a path and say so.

**A mount answers for its whole prefix, misses included.** A registry or struct mounted at `/x` frames its own `MethodNotFound` for `/x/absent`; resolution stops at the first prefix that matches and the fallback is never reached. A fallback sees only paths no mount covers, so a plugin whose claimed root overlaps a mounted struct is shadowed by it.

A fallback takes an `Arc<dyn HandlerErased>`: it receives the raw request and returns a whole `Message`, because deciding what to do with a miss usually starts with reading the query. `with_fallback_blocking` is the off-reader variant, for a miss handler that leaves the process — a plugin call across a C ABI, a proxied request to another node — where the WebSocket server should not tie up its reader task waiting.

### Unknown request-body keys

repe-rs ignores object keys it does not recognize when decoding a request body into a typed handler (`with_typed`) or a registered struct, across both JSON and BEVE. This is a deliberate, guaranteed forward-compatibility property, not an accident of the codec: a newer client can add an optional field to a request and an older server built against this crate decodes the rest and drops the unknown key rather than rejecting the call. See [Schema Evolution](protocol.md#schema-evolution) for the protocol stance.

A handler that wants the opposite — reject a request carrying undeclared keys, for instance to catch a client typo — opts in per type with serde's `#[serde(deny_unknown_fields)]` on its request struct; the rejection then names the offending key. Strictness is a per-type decision the handler author makes, never the server default.

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

`register_struct` exposes a struct's fields and methods through JSON Pointer paths automatically. `#[derive(RepeStruct)]` reflects the **fields**; `#[repe::methods]` on an inherent `impl` block reflects the **methods**. Mark nested struct fields with `#[repe(nested)]`.

```rust
use repe::Router;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods)]
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

#[repe::methods]
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
- `/device` reads the whole object, with each method published as its signature string.

### The two halves of the surface

A derive macro is attached to the struct definition and cannot see `impl` blocks, so the two macros are tied together by a compile-time handshake: `#[repe(methods)]` on the struct says "my methods live in a `#[repe::methods]` block", and the attribute macro asserts that marker back. Leave either half off and the build fails naming the attribute that is missing — a method can never be quietly absent from the served surface.

Everything in an annotated block becomes an endpoint, with two exceptions that need no annotation:

- **associated functions** (`fn new() -> Self`) — there is no instance to dispatch on, so they are simply not endpoints;
- **`#[repe(skip)]`** methods, for anything else you want to keep off the wire.

`#[repe(rename = "...")]` publishes a method under a different path segment. A method that consumes `self`, is `async`, `unsafe`, generic, takes a reference argument, or is behind a `#[cfg]` is a compile error rather than a silent omission; add `#[repe(skip)]` if it belongs in the block but not on the wire.

`#[cfg]` is worth calling out because the reason is not obvious: conditional compilation is applied *after* attribute macros run, so the macro would publish an endpoint for a method that may not exist. Put a conditionally-compiled method in a plain `impl` block, or `#[repe(skip)]` it and publish an unconditional wrapper.

Two declarations claiming the same endpoint — two fields, two methods, or a field and a method — are also rejected. One of them would win dispatch and the other would be unreachable forever, and the whole-object listing would carry the key twice.

### Arguments

A method's arguments are deserialized from the request body:

| Arity | Body |
|---|---|
| 0 | ignored |
| 1 | *is* the argument |
| 2+ | an array of N values, positionally, **or** an object keyed by parameter name |

```rust
#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods)]
struct Mixer { gain: f64 }

#[repe::methods]
impl Mixer {
    fn blend(&self, left: f64, right: f64) -> f64 { self.gain * (left + right) }
}
```

`/blend` accepts `[1.0, 3.0]` or `{"left": 1.0, "right": 3.0}`. In the object form a missing key decodes as `null`, so an `Option<T>` parameter may be omitted.

### Fallible methods

A method returning `Result<T, E>` sends `Ok(v)` as the payload and turns `Err(e)` into an error frame carrying `e.to_string()`, rather than serializing the `Result` itself.

The check is **name-based**: a macro sees a type, not a resolved one, so it matches anything whose last path segment is `Result` with one or two type arguments. That covers `Result<T, E>`, `std::result::Result<T, E>`, and the widespread one-parameter aliases — `anyhow::Result<T>`, `std::io::Result<T>`, a crate's own `pub type Result<T> = ...`.

It has a boundary in each direction, and the second one matters:

- A type of your own that is *named* `Result` but is not one is treated as fallible. You will notice: it fails to compile.
- A `Result` aliased under **another name** (`type DeviceResult<T> = Result<T, DeviceError>`) is **not** recognized, and is serialized as data — so `Err` reaches the client as a *success* frame carrying `{"Err": ...}`. Nothing warns about this. If you use such an alias, spell the return type as `Result<..>` on any method you publish.

Resolving either would need type information a macro does not have.

### Field-shaped endpoints

Some values look like fields to a client and are not fields at all: a register in different units, a value derived from two others, a setting the hardware holds rather than the struct. `#[repe(get = "...")]` and `#[repe(set = "...")]` publish one endpoint served by a getter/setter pair:

```rust
#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods)]
struct Synthesizer {
    phase_step: u32,
    sample_rate_mhz: f64,
}

#[repe::methods]
impl Synthesizer {
    #[repe(get = "mix_freq_mhz")]
    fn mix_freq_mhz(&self) -> f64 {
        self.phase_step as f64 * self.sample_rate_mhz / 4096.0
    }

    #[repe(set = "mix_freq_mhz")]
    fn set_mix_freq_mhz(&mut self, mhz: f64) {
        self.phase_step = (mhz * 4096.0 / self.sample_rate_mhz).round() as u32;
    }

    /// No setter: read-only, with nothing extra to say.
    #[repe(get = "firmware")]
    fn firmware(&self) -> &'static str { "1.4.2" }
}
```

`/mix_freq_mhz` now behaves exactly like a field: a bodiless request reads it, a request with a body writes it, a write is acknowledged with `null`, and the whole-object read lists its **value** rather than a signature string. Publishing the same thing as methods would mean `/get_mix_freq_mhz` and `/set_mix_freq_mhz`, which is a different path and a different shape.

The two halves need not sit next to each other, and the rules are the ones the shape implies:

- a getter takes no arguments and returns the value; a setter takes exactly one argument and returns `()` or `Result<(), E>`;
- either half may be fallible, and `Err` becomes an error frame as it does for any published method;
- a getter with **no** setter is read-only, and a write to it returns `InvalidBody` — the same refusal `#[repe(readonly)]` gives, without a no-op setter written to stand in for one;
- a setter with no getter is a compile error: the whole-object listing would have no value to show for the endpoint;
- `#[repe(typed)]` composes, on the getter, exactly as it does on a field.

`#[repe(skip)]` and `#[repe(rename = "...")]` are rejected here rather than silently ignored: the endpoint is the name given to `get`/`set`, and skipping half a pair has no meaning. Accessor endpoints take part in the same collision check as fields and methods.

### Reflection, and its floor

Adding a **field** to the struct, or a **method** to the annotated block, adds the endpoint. Nothing restates a signature, so the served surface cannot fall behind the type.

Worth stating plainly: Rust cannot reach Glaze's zero-annotation reflection. Glaze counts and names the members of any aggregate with nothing added to the type; `repr(Rust)` has no defined layout and no stable field enumeration, so every Rust path runs through a macro that has seen the definition. One derive per struct, plus one attribute per impl block, is the floor.

The struct-level list form remains as the escape hatch for a block that cannot be annotated — a foreign type, or an impl generated by another macro:

```rust
#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods(
    greet(&self) -> String,
    blend(&self, left: f64, right: f64) -> f64,
    alias = greet(&self) -> String
))]
struct Legacy { gain: f64 }

impl Legacy {
    fn greet(&self) -> String { format!("gain {}", self.gain) }
    fn blend(&self, left: f64, right: f64) -> f64 { self.gain * (left + right) }
}
```

It supports the same arities and the same `Result` mapping, and the two forms may be used on the same struct. It carries the drift the impl-block form removes, though: renaming a listed method or changing its types is a compile error, but *adding* one to the impl block leaves it off the wire. Prefer `#[repe::methods]` where you can annotate the block.

Declare the receiver accurately. A method listed as `&self` is served through the shared read path below, so listing a `&mut self` method as `&self` is a compile error in the generated code rather than a silently exclusive endpoint.

### Field attributes

| Attribute | Effect |
|---|---|
| `#[repe(rename = "...")]` | publish under a different path segment |
| `#[repe(skip)]` | keep the field off the wire entirely |
| `#[repe(readonly)]` | reads succeed, writes return `InvalidBody` |
| `#[repe(nested)]` | descend into a field that is itself a `RepeStruct` |
| `#[repe(typed)]` | encode a numeric slice as a BEVE typed array |

For a field-shaped endpoint with no field behind it, see [Field-shaped endpoints](#field-shaped-endpoints).

`#[repe(typed)]` routes a numeric array or `Vec` field to the bulk encoder behind [`MessageBuilder::body_typed_slice`](numeric-bodies.md) — one `copy_nonoverlapping` rather than a per-element serde walk, and byte-identical to what Glaze emits for the same array. That response carries `BodyFormat::Beve`; decode it with `Message::decode_typed_slice`. Writes to the field are unaffected and still take JSON. Inside the whole-object read the frame is already committed to JSON, so the field appears there as an ordinary JSON array; the typed encoding is what you get by reading the field on its own, which is the case it exists for.

### Reads do not build an intermediate `Value`

A read serializes the live field straight into the outgoing frame buffer. `RepeStruct` carries two methods for this: `repe_handle`, which returns a `serde_json::Value` and is all a hand-written impl needs, and `repe_handle_into`, which encodes in place and is what the router calls. The derive generates both; the default `repe_handle_into` falls back to `repe_handle`, so an existing hand-written impl keeps working unchanged and can override the second method when it is worth it.

Two consequences worth knowing:

- Reading a whole object no longer walks a `serde_json::Map`, so its keys come out in **declaration order** rather than sorted. Previously the order came from `serde_json::Map`, which is alphabetical unless something in the dependency graph enables `serde_json/preserve_order` — declaration order is both stable and what Glaze emits.
- `#[repe(typed)]` only takes effect on the encoding path; `repe_handle` still yields a JSON array, since a `Value` cannot carry a BEVE typed body.

`Router` accepts `Arc<L>` for any lock implementing `repe::Lockable<T>`, so you can swap in `tokio::sync::Mutex` / `RwLock` (via their `blocking_*` APIs) or enable the optional `parking-lot` feature to use `parking_lot::Mutex` / `RwLock` without extra wrapper types.

### Reads share the guard

A read does not need exclusive access, and behind an `RwLock` it no longer takes it. REPE separates a read from a write at the frame level — a request with no body is a read — so the router can decide before it locks anything:

```rust
use repe::server::Router;

# #[derive(Default, serde::Serialize, serde::Deserialize, repe::RepeStruct)]
# struct Instrument { gain: f64 }
let (router, instrument) = Router::new().with_struct_rw("/instrument", Instrument::default());
```

`with_struct_rw` is `with_struct` behind an `RwLock` instead of a `Mutex`; `with_struct_shared` takes any `Arc<L>` where `L: repe::Lockable<T>`, including `tokio::sync::RwLock` and, with the `parking-lot` feature, `parking_lot::RwLock`.

Under that registration a `/instrument/gain` read runs concurrently with any other read, including one already inside a slow `&self` method. What is served this way is everything the derive can reach without `&mut self`:

- every field read, at any depth through `#[repe(nested)]`;
- a `#[repe::methods]` method taking `&self` and no arguments;
- the getter half of a field-shaped endpoint, when that getter takes `&self`;
- the whole-object listing, when nothing in it has to be *invoked* — see below.

Everything else — every write, every `&mut self` method, a `&mut self` getter — **declines**: the shared attempt returns without writing anything, and the router retakes the lock exclusively and dispatches exactly as it always did. Declining is invisible from the outside; the answer is the same frame either way.

`with_struct` puts the value behind a `Mutex`, which has no shared mode. There the shared path is compiled out entirely rather than acquiring the same lock twice, so a mutex-backed struct dispatches exactly as it did before.

#### A listing never invokes a getter

A whole-object read is the one read that composes many others, so a decline discovered partway through would leave the entries before it already executed — and the exclusive retry then executes them again. A `&self` getter over a read counter would report the second call.

So a struct that publishes any field-shaped endpoint declines its whole-object listing outright, before writing or calling anything. Everything left in a shared listing is serialization, which re-runs harmlessly. Reading one of those endpoints on its own is unaffected: that decision comes from the receiver, before anything is called.

The trade is deliberate. A struct with accessors keeps the shared path for every individual read and gives it up only for the whole-object read, which is a discovery request rather than a hot one — and is [already quadratic in the accessor count](#field-shaped-endpoints) for the same reason.

#### Two things that change for callers

- **Two `&self` methods on one object now run at the same time.** The exclusive guard used to give them mutual exclusion for free. A `&self` method that reads several interior-mutable cells expecting a consistent snapshot needs its own synchronization now. Writers still exclude readers.
- **A panic under a shared guard no longer poisons the lock.** `std::sync::RwLock` poisons only on a panic while the *write* guard is held, so a panicking `&self` handler no longer retires the object for the life of the process the way it did under a `Mutex`. A panicking `&mut self` handler still does.

A hand-written `RepeStruct` impl gets the exclusive behavior by default. To opt one in, override `repe_read_into`, which carries two obligations: answer a path identically to `repe_handle_into` or decline it, and write nothing into the response body when declining. `ObjectBody::entry_try_with` is there for the second one when the decline surfaces partway through an object — it rewinds the whole object, so propagating its `None` is all the caller has to do.

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

The built-in WebSocket server constructs a `PeerHandle` per connection and threads it into each request's `CallContext`, so over WebSocket you write only the context-aware handler and read `ctx.peer()` — no sink or dispatch wiring of your own. The TCP servers and direct in-process dispatch do not attach a peer; there `ctx.peer()` returns `None`, and you wire your own `PeerSink` (typically a bounded channel drained by a writer task) and call `Registry::dispatch_with_ctx` with a populated `CallContext`, as in the example below.

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
