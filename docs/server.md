# Server, Routers, and Handlers

`Router` maps JSON Pointer paths to handler closures or typed services. `Server` and `AsyncServer` accept TCP connections and dispatch them through a router. `WebSocketServer` (behind the `websocket` feature) does the same over WebSocket.

## Router

Add handlers with `.with_typed(path, fn)`. Every route names the type it takes and the type it returns: the request body is decoded straight into the parameter and the return value is written straight into the response frame, with no intermediate document either way. JSON, UTF-8, and BEVE bodies all reach the same handler — the frame header says which — and the response defaults to JSON; wrap the return with `TypedResponse::beve(...)` / `TypedResponse::utf8(...)` to pick a different response `BodyFormat`. Bodies in unsupported formats are rejected with `Invalid body`.

There is no untyped handler. `with_json` took a `serde_json::Value` in and out; with no document model there is nothing for it to be, and a route that names its types turns a wrong body into a decode error at the boundary rather than a `None` found inside the handler.

Declare a body type with `structio::object!`, which is a `macro_rules!` macro — no derive, no proc macro, and it works on a type you do not own.

Pre-request middleware runs before the handler and can centralize auth, validation, or tracing.

```rust
use repe::{Router, Server};
use std::time::Duration;

let router = Router::new()
    .with_middleware(|req, next| {
        if let Ok(path) = req.query_str() {
            println!("incoming request for {path}");
        }
        next.run(req)
    })
    .with_typed("/ping", |_: Empty| Ok(Pong { pong: true }))
    .with_typed("/echo", |v: Message| Ok(v))
    .with_typed("/status", |_: Empty| Ok(Status { status: "ok".into() }));

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
    .with_typed("/ping", |_: Empty| Ok(Pong { pong: true }))
    .with_fallback(Arc::new(dynamic_table));
```

It is the natural mount for anything whose table is built while the server runs: a [plugin](plugins.md#mounting-a-plugin-on-a-router) loaded on demand, a proxy to another node, a registry mounted after startup. There was no way to do this before, not even an awkward one — middleware is attached per route, so on a miss no pipeline exists and none of it runs.

The handler owns every miss it is given: nothing else will answer, so one that does not claim the request has to frame `MethodNotFound` itself — which is also what lets it decline a path and say so.

**A mount answers for its whole prefix, misses included.** A registry or struct mounted at `/x` frames its own `MethodNotFound` for `/x/absent`; resolution stops at the first prefix that matches and the fallback is never reached. A fallback sees only paths no mount covers, so a plugin whose claimed root overlaps a mounted struct is shadowed by it.

That has a degenerate case worth knowing before it is met: **a mount at the empty root matches every path**, so it does not merely narrow the fallback, it makes it unreachable. That is the ordinary registration for a service ported from Glaze's `glz::asio_server::on(*this)`, where the whole object hangs off the top level and the path shape is the client contract, so the root cannot be moved to buy the fallback back.

`Router::with_mount_fallthrough()` is the answer: a mount that would frame `MethodNotFound` hands the request to the fallback instead.

```rust
let (router, state) = Router::new().with_struct_rw("", instrument);
let router = router
    .with_fallback(Arc::new(plugin_table))
    .with_mount_fallthrough();
```

The rule is uniform — a mount's miss is still a miss, at the empty root or at any prefix — and nothing is reordered: a fixed route still beats a mount, and a mount still beats the fallback for a path it serves. Registration order does not matter; the composition is rebuilt whenever the mounts, the fallback, the middleware chain, or the flag changes. Middleware wraps the composite rather than each half, so a request that falls through runs the pipeline once.

Two things it does not change. The fallback still owns every miss it is given, so one that does not claim the request must still frame `MethodNotFound` itself; and the mount's own diagnostic for the path is replaced by whatever the fallback frames. It is opt-in for that second reason — a host that mounts at a prefix may prefer the mount's more specific error.

A fallback takes an `Arc<dyn HandlerErased>`: it receives the raw request and returns a whole `Message`, because deciding what to do with a miss usually starts with reading the query. `with_fallback_blocking` is the off-reader variant, for a miss handler that leaves the process — a plugin call across a C ABI, a proxied request to another node — where the WebSocket server should not tie up its reader task waiting.

### Unknown request-body keys

repe-rs ignores object keys it does not recognize when decoding a request body into a typed handler (`with_typed`) or a registered struct, across both JSON and BEVE. This is a deliberate, guaranteed forward-compatibility property, not an accident of the codec: a newer client can add an optional field to a request and an older server built against this crate decodes the rest and drops the unknown key rather than rejecting the call. See [Schema Evolution](protocol.md#schema-evolution) for the protocol stance.

It is written down as `repe::WirePolicy`, the read policy every body decode in this crate uses. structio's own default is the opposite — an unknown key is refused — which is the right default for a document you own and the wrong one for a frame that arrived from someone else's build.

A handler that wants the opposite — reject a request carrying undeclared keys, to catch a client typo — reads the body itself with `RequestBody::read_into_with::<structio::Standard, _>(..)`. That is a deliberate narrowing of what the protocol permits, decided per endpoint rather than server-wide.

## Typed Handlers via `JsonTypedHandler`

Implement the `JsonTypedHandler` trait to attach a service type's methods to a router:

```rust
use repe::{Router, JsonTypedHandler, ErrorCode};

#[derive(Default)]
struct Input { name: String }
structio::object!(Input { name });

#[derive(Default)]
struct Output { greeting: String }
structio::object!(Output { greeting });

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

// `#[derive(RepeStruct)]` publishes the endpoints; `structio::object!` gives
// the type its wire encoding. A served type needs both, and they are separate
// on purpose: a type can have an encoding without being served, and the
// declaration works on a type from a crate you do not own.
#[derive(Default, repe::RepeStruct)]
#[repe(methods)]
struct Device {
    id: String,
    status: String,
    #[repe(nested)]
    metrics: Metrics,
}
structio::object!(Device { id, status, metrics });

#[derive(Default, repe::RepeStruct)]
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

In the object form a missing key decodes as `null`, so a parameter typed `Option<T>` may be omitted and arrives as `None`. Omitting a parameter that is not optional is an `InvalidBody` error naming it — `deserialization error for /blend(right)` — rather than a silent default.

```rust
#[derive(Default, repe::RepeStruct)]
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

Some values look like fields to a client and are not fields at all: a stored value in different units, a value derived from two others, a setting the backing resource holds rather than the struct. `#[repe(get = "...")]` and `#[repe(set = "...")]` publish one endpoint served by a getter/setter pair:

```rust
#[derive(Default, repe::RepeStruct)]
#[repe(methods)]
struct Budget {
    used_bytes: u64,
    total_bytes: u64,
}

#[repe::methods]
impl Budget {
    #[repe(get = "used_percent")]
    fn used_percent(&self) -> f64 {
        self.used_bytes as f64 * 100.0 / self.total_bytes as f64
    }

    #[repe(set = "used_percent")]
    fn set_used_percent(&mut self, percent: f64) {
        self.used_bytes = (percent * self.total_bytes as f64 / 100.0).round() as u64;
    }

    /// No setter: read-only, with nothing extra to say.
    #[repe(get = "version")]
    fn version(&self) -> &'static str { "1.4.2" }
}
```

`/used_percent` now behaves exactly like a field: a bodiless request reads it, a request with a body writes it, a write is acknowledged with `null`, and the whole-object read lists its **value** rather than a signature string. Publishing the same thing as methods would mean `/get_used_percent` and `/set_used_percent`, which is a different path and a different shape.

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
#[derive(Default, repe::RepeStruct)]
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
| `#[repe(readonly)]` | reads succeed, every write *through* the field returns `InvalidBody` |
| `#[repe(nested)]` | descend into a field that is itself a `RepeStruct` |
| `#[repe(typed)]` | encode a numeric slice as a BEVE typed array |

For a field-shaped endpoint with no field behind it, see [Field-shaped endpoints](#field-shaped-endpoints).

### Struct attributes

| Attribute | Effect |
|---|---|
| `#[repe(methods)]` | the method table comes from a `#[repe::methods]` impl block |
| `#[repe(methods(..))]` | the method table is the list given here |
| `#[repe(no_replace)]` | a write of the **whole object** is refused, and `Self: DeserializeOwned` is not required |
| `#[repe(listing_order(..))]` | the key order of the whole-object listing, named in full |

`#[repe(no_replace)]` refuses a write of the whole object and nothing more. It is spelled apart from `#[repe(readonly)]` because the two are different statements: `readonly` on a field says *this subtree is not writable*, recursively, while `no_replace` says *this type cannot be rebuilt from a body* and leaves every field writable. Writing `#[repe(readonly)]` on a struct is a compile error naming both, rather than silently meaning one of them. It also matters for more than the refusal. Without it the empty-segments arm reads the body into `*self`, so **every** derived struct has to have a structio declaration — including one that holds an open socket, a file handle, or anything else no document describes. The attribute emits only the refusal, never the assignment followed by dead code, so the read is not generated and the declaration is not required. Fields and children are unaffected: they still read and write as they always did.

`#[repe(listing_order(..))]` names every key of the whole-object listing, in the order to emit them. Without it the listing appends fields in declaration order, then struct-listed methods, then the impl block's signatures, then its field-shaped accessors — so a `#[repe(get/set)]` endpoint is always last, wherever its logical place reads. That is wire-visible, and it is the one key order a `glz::object` with a `custom<setter, getter>` in the middle cannot be reproduced in.

```rust
#[derive(Default, repe::RepeStruct)]
#[repe(methods)]
#[repe(listing_order("name", "count", "percent", "total", "identify"))]
struct Report {
    name: String,
    count: u32,
    total: f64,
    #[repe(skip)]
    ratio: f64,
}

#[repe::methods]
impl Report {
    #[repe(get = "percent")]
    fn percent(&self) -> f64 { self.ratio * 100.0 }
    #[repe(set = "percent")]
    fn set_percent(&mut self, percent: f64) { self.ratio = percent / 100.0; }
    fn identify(&self) -> String { self.name.clone() }
}
```

Naming the sequence in full is what makes a typo or an omission a compile error. Half the check is at macro time, against the fields and the struct-level method list, with the offending key in the message; the other half is a `const` assertion against the impl block's two generated tables, which the derive cannot see. It reorders emission only — same keys, same values, no run-time cost, and nothing at all for a struct that does not use it.

The order governs every frame that reaches a client, because a listing is written member by member into the response buffer. It is assertable, too: previously the listing was assembled into a `serde_json::Map` — a `BTreeMap` unless something in the graph enabled `preserve_order` — which sorted the keys and could carry no order at all, so neither this attribute's order nor declaration order survived to the wire.

### Descending into a child

`#[repe(nested)]` asks the field's own `RepeStruct` impl. Every path through the field goes there, including a write of the **whole child**: `/device/metrics` with body `{"temperature": 22.0}` is handed to `Metrics::repe_handle_into(&[], Some(body))` rather than assigned over the field.

That matters for a child that owns something. A derived child's empty-segments arm reads the body into `*self`, so a derived child is still replaced; a hand-written one can *apply* the fields it was given, which is what a live settings object means by a write. `#[repe(readonly)]` on the field refuses every write through it, whole-child and sub-path alike.

There used to be a second attribute here, `#[repe(nested_serde)]`, for a field whose type implemented only `Serialize` + `DeserializeOwned` — the case where the crate that *declares* the type cannot depend on this one. It descended by materializing the field as a `serde_json::Value` and walking it, which cost a round trip on every sub-path and could only address the serialized names.

It is gone, and nothing replaced it because nothing needs to. `structio::object!` is a `macro_rules!` macro, so a declaration can be written for a type from a crate you do not own, in the crate that uses it; and [`repe-core`](#repe-core-declaring-a-served-type-without-the-server) carries `RepeStruct` without the server, the client, or the transport, so a pure-logic crate can implement it without acquiring an RPC layer. `#[repe(nested)]` then descends without materializing anything.

A child that may not be there at all is an `Option<T>`. `RepeStruct` is implemented for it here, because a host cannot implement it itself — `Option` is foreign and the trait is not theirs. Present forwards everything to the inner value; absent reads as `null` at the child's own path and answers `MethodNotFound` to a write or any sub-path, because a silent no-op against a live resource is worse than an error. Presence itself is not settable through that path: a `null` write is refused whether the child is there or not, because removing one would otherwise be a door that only opens one way. A whole-object write at the parent still replaces the field.

```rust
# #[derive(Default, repe::RepeStruct)]
# struct Metrics { temperature: f64 }
#[derive(Default, repe::RepeStruct)]
struct Device {
    id: String,
    #[repe(nested)]
    metrics: Option<Metrics>,
}
```

`#[repe(typed)]` routes a numeric array or `Vec` field to the bulk encoder behind [`MessageBuilder::body_typed_slice`](numeric-bodies.md) — one `copy_nonoverlapping` rather than a per-element walk, and byte-identical to what Glaze emits for the same array. That response carries `BodyFormat::Beve`; decode it with `Message::decode_typed_slice`. Writes to the field are unaffected and still take JSON. Inside the whole-object read the frame is already committed to JSON, so the field appears there as an ordinary JSON array; the typed encoding is what you get by reading the field on its own, which is the case it exists for.

### Nothing is materialized in either direction

`RepeStruct` has one dispatch method, `repe_handle_into`. It is handed the request body as the bytes that arrived plus the format the header declared, and an output buffer to write into. A body is parsed **once, directly into the live member it is destined for**, and a response is written straight into the outgoing frame. Nothing allocates until a reader does, and a request no endpoint claims never parses at all.

There used to be a second method, `repe_handle`, which took an `Option<serde_json::Value>` and returned one. It is gone with the document model, and the shape it forced went with it: a request was parsed into a tree, walked, and re-parsed into the member; a response was built as a tree and then serialized.

Two consequences worth knowing:

- Reading a whole object writes its members in **declaration order**, and that order reaches the wire. Previously the listing was assembled into a `serde_json::Map`, which is alphabetical unless something in the dependency graph enables `preserve_order`.
- `#[repe(typed)]` takes effect wherever the field owns its own frame. Inside a listing the frame is already committed to JSON, so the field appears there as an ordinary JSON array.

`Router` accepts `Arc<L>` for any lock implementing `repe::Lockable<T>`, so you can swap in `tokio::sync::Mutex` / `RwLock` (via their `blocking_*` APIs) or enable the optional `parking-lot` feature to use `parking_lot::Mutex` / `RwLock` without extra wrapper types.

### Reads share the guard

A read does not need exclusive access, and behind an `RwLock` it no longer takes it:

```rust
use repe::server::Router;

# #[derive(Default, repe::RepeStruct)]
# struct Instrument { gain: f64 }
let (router, instrument) = Router::new().with_struct_rw("/instrument", Instrument::default());
```

`with_struct_rw` is `with_struct` behind an `RwLock` instead of a `Mutex`; `with_struct_shared` takes any `Arc<L>` where `L: repe::Lockable<T>`, including `tokio::sync::RwLock` and, with the `parking-lot` feature, `parking_lot::RwLock`.

Under that registration a `/instrument/gain` read runs concurrently with any other read, including one already inside a slow `&self` method. What is served this way is everything the derive can reach without `&mut self`:

- every field read, at any depth through `#[repe(nested)]`;
- **every `#[repe::methods]` method taking `&self`, arguments included**;
- the getter half of a field-shaped endpoint, when that getter takes `&self`;
- a refusal that needs no state at all — a write to a `#[repe(readonly)]` endpoint;
- the whole-object listing, when nothing anywhere beneath it has to be *invoked* through `&mut self` — see below.

**The receiver decides, not the frame.** REPE separates a read from a write at the frame level, and taking that as the borrow rule looks right until a `&self` method takes arguments: the frame then carries a body, so it was dispatched exclusively and stalled every read of the object for as long as it ran. A long-running `&self` call turned a sub-millisecond `/version` read into one that waited for the whole call — a regression against the C++ registry this replaces, which has no mutex at all. The receiver is known where the dispatch arms are generated, so it is the receiver that answers.

Everything else — every field write, every `&mut self` method, a `&mut self` getter, a setter — **declines**: the shared attempt returns without writing anything and without consuming the request body, and the router retakes the lock exclusively and dispatches exactly as it always did. Declining is invisible from the outside; the answer is the same frame either way.

`with_struct` puts the value behind a `Mutex`, which has no shared mode. There the shared path is compiled out entirely rather than acquiring the same lock twice, so a mutex-backed struct dispatches exactly as it did before.

A frame carrying a body skips the shared attempt entirely when the struct cannot answer one. A body is what a write and a call-with-arguments have in common, so the frame alone cannot separate them, but the type can: `RepeStruct::REPE_SHARED_SERVES_BODIES` is `false` for a struct whose every write needs `&mut self`, and the router then goes straight to the exclusive lock rather than taking the read lock to be told no. The derive computes it — `true` for a `&self` method taking arguments, for any `#[repe(readonly)]` endpoint whose refusal needs no state, and for any `#[repe(nested)]` child with either, at any depth. It defaults to `true`, so a hand-written `repe_shared_into` keeps being asked, and it is only ever a hint: whatever the shared borrow would have answered with a body, the exclusive path answers identically.

#### A listing decides at the top, or not at all

A whole-object read is the one read that composes many others, so a decline discovered partway through would leave the entries before it already executed — and the exclusive retry then executes them again. A `&self` getter over a read counter would report the second call.

So the listing settles the question before it writes or calls anything, and it settles it for the **whole subtree**. Two things can force the decline:

- an accessor on the struct itself whose getter takes `&mut self` — fields serialize and published methods are listed by their signature, so a getter is the only listing entry that is *invoked*;
- a `#[repe(nested)]` child that declines, at any depth, since a parent's listing composes its children's.

The subtree half is not optional. A child is listed before the parent's own accessors are read, so a parent that only checked its own getters would list a child, invoke that child's getters, and *then* discover a later sibling declining — leaving the retry to invoke them a second time. Rewinding the response buffer undoes the bytes; it cannot undo a call.

So a struct whose getters all take `&self`, and whose children are the same — the ordinary case — keeps its shared whole-object listing, with each accessor's current value in it. One `&mut self` getter anywhere beneath gives it up. Reading an individual endpoint is unaffected either way; that decision comes from the receiver, before anything is called.

Two overridables carry it, both defaulting to the conservative answer. `RepeMethods::REPE_LISTING_NEEDS_EXCLUSIVE` is the per-table half — any accessor at all, unless `#[repe::methods]` computed otherwise from the receivers it has seen. `RepeStruct::repe_listing_declines` is the subtree half, which a parent asks of each child; it defaults to `true`, the accurate answer for the default `repe_shared_into`, which declines everything. A hand-written impl that serves listings shared should override it, or every derived struct nesting it gives up its own listing. Overriding either is a promise: break it and the response is still correct — the listing rewinds and retries exclusively — but whatever the shared attempt had already invoked runs twice.

#### Two things that change for callers

- **Two `&self` methods on one object now run at the same time.** The exclusive guard used to give them mutual exclusion for free. A `&self` method that reads several interior-mutable cells expecting a consistent snapshot needs its own synchronization now. Writers still exclude readers.
- **A panic under a shared guard no longer poisons the lock.** `std::sync::RwLock` poisons only on a panic while the *write* guard is held, so a panicking `&self` handler no longer retires the object for the life of the process the way it did under a `Mutex`. A panicking `&mut self` handler still does.

A hand-written `RepeStruct` impl gets the exclusive behavior by default. To opt one in, override `repe_shared_into`, which carries three obligations:

- **answer a path identically** to `repe_handle_into`, or decline it;
- **write nothing into the response body when declining.** `ObjectBody::entry_try_with` is there for this when the decline surfaces partway through an object — it rewinds the whole object, so propagating its `None` is all the caller has to do;
- **leave the body alone when declining.** It arrives as `Option<RequestBody<'_>>`, a `Copy` view of the request bytes, so the exclusive retry re-dispatches the very same request at no cost. Read from it only once this borrow has committed to answering — an `Err` counts as an answer — and never on a path that goes on to return `None`, because a read lands in a live member and the retry would then apply it twice.

### `repe-core`: declaring a served type without the server

A served type's endpoints are declared where the *type* is declared, and that is not always the crate that runs the RPC. A pure-logic crate — no I/O, no `unsafe`, buildable on any host, and often kept that way deliberately — should not have to acquire a server, a client, and a transport just to publish a couple of paths.

`repe-core` carries the `RepeStruct` surface on its own: the trait, `RepeMethods`, `StructError`, `ResponseBody`, and the protocol constants those name. Depend on it from the crate that declares the type, and on `repe` from the crate that serves it.

```toml
# the pure crate
[dependencies]
repe-core = "4"
# The declaration macro. Zero dependencies, no proc macro, no build script.
structio = "0.2"
```

```rust,ignore
use repe_core::RepeStruct;

#[derive(Default, RepeStruct)]
pub struct Build {
    pub version: String,
    pub revision: u64,
}
structio::object!(Build { version, revision });
```

`repe` re-exports every one of those at the paths they have always had — `repe::structs::*`, `repe::constants::*` — so a type derived against `repe-core` mounts on a `repe::Router` with nothing in between, and `#[derive(RepeStruct)]` resolves its generated paths against whichever of the two crates is in scope. The one spelling that follows the crate name is the attribute macro: `#[repe_core::methods]` where only the core is a dependency, `#[repe::methods]` where `repe` is. It is one macro either way.

`repe-core` has no features. It used to gate the BEVE typed-numeric encoding behind a `typed` feature, because `beve` and the six packages behind it were the whole weight of the crate; `structio` has no dependencies, so there is nothing left to gate and `#[repe(typed)]` costs nothing to have available.

Where the type is one you do not own, declare it where you use it: `structio::object!` is a `macro_rules!` macro, so it needs no access to the type's own crate.

## Async Server

`AsyncServer` mirrors `Server` and runs on tokio. See `examples/async_server.rs`.

```rust
use repe::{AsyncServer, Router};

# async fn run() -> std::io::Result<()> {
let router = Router::new().with_typed("/ping", |_: Empty| Ok(Pong { pong: true }));
let listener = AsyncServer::listen(("127.0.0.1", 0)).await?;
tokio::spawn(async move { let _ = AsyncServer::new(router).serve(listener).await; });
# Ok(()) }
```

## Peer-Aware Handlers

Handlers that need to push more than one message back to the calling client (e.g. server-pushed file chunks after a single `/run_collection` call) need a typed handle to that connection. `PeerSink` / `PeerHandle` / `CallContext` provide that handle, and `Registry::call` threads it through to the handler.

The built-in WebSocket server constructs a `PeerHandle` per connection and threads it into each request's `CallContext`, so over WebSocket you write only the context-aware handler and read `ctx.peer()` — no sink or dispatch wiring of your own. The TCP servers and direct in-process dispatch do not attach a peer; there `ctx.peer()` returns `None`, and you wire your own `PeerSink` (typically a bounded channel drained by a writer task) and call `Registry::call` with a populated `CallContext`, as in the example below.

```rust
use repe::{
    CallContext, NotifyBody, PeerHandle, PeerId, PeerSendError, PeerSink,
    Registry, WithContext,
};
use std::sync::Arc;

#[derive(Default)]
struct Status { status: String }
structio::object!(Status { status });

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
    Ok::<_, (repe::ErrorCode, String)>(Status { status: "ok".into() })
})).unwrap();

let peer = PeerHandle::new(PeerId(1), Arc::new(OutboundChannel(/* ... */)));
let ctx = CallContext::new("/run", &peer);
let mut buf = Vec::new();
let mut out = repe::ResponseBody::new(&mut buf);
let _ = registry.call("/run", None, &ctx, &mut out);
```

`WithContext` is the marker that opts a closure into the `&CallContext` parameter. Plain `Fn(Option<RequestBody<'_>>) -> Result<...>` handlers keep working unchanged: `Registry::call_detached` is a thin wrapper that supplies a `CallContext::detached` context.
