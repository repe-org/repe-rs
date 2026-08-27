# Plugins

A REPE **plugin** is a shared library that a host loads at runtime and drives over a small C ABI. The payloads crossing the boundary are ordinary REPE frames, so a plugin is just a router that happens to live behind `dlopen` instead of behind a socket.

The ABI is defined by [Glaze](https://github.com/stephenberry/glaze) in `glaze/rpc/repe/plugin.h`, and this crate implements both ends of it. A `cdylib` built here loads into an existing C++ REPE host with no adapter on either side, which is the cheapest way to introduce Rust into an established C++ deployment: one leaf at a time, without touching the host. A Rust host loads C++ plugins the same way, which is what makes the reverse migration possible — port the host and keep the plugins.

| | Feature | What it gives you |
|---|---|---|
| **Build a plugin** | `plugin` | `#[repe::plugin]`, which turns a `Router` into the five C exports |
| **Host plugins** | `plugin-host` | `plugin::host::Plugin`, which loads a library and drives it |

## Building a plugin

Requires the `plugin` feature. The library must be a `cdylib` — that is what produces a `.so`/`.dylib`/`.dll` with resolvable symbols.

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
repe = { version = "9", features = ["plugin"] }
```

```rust
use repe::server::Router;

#[repe::plugin(root = "/instrument")]
fn build() -> Router {
    Router::new().with_struct("/instrument", Instrument::default()).0
}
```

That is the whole plugin. The attribute emits the five exports a host resolves after `dlopen`:

| Symbol | Purpose |
|---|---|
| `repe_plugin_interface_version` | ABI version, checked **before** anything else is read |
| `repe_plugin_info` | name, version, and the RPC path prefix this plugin claims |
| `repe_plugin_init` | build the router (optional for the host to call) |
| `repe_plugin_shutdown` | refuse further requests |
| `repe_plugin_call` | dispatch one REPE request frame |

`name` and `version` default to the crate's `CARGO_PKG_NAME` and `CARGO_PKG_VERSION`, so the plugin's identity comes from the manifest that already carries it. `root` is required and must be an absolute JSON Pointer prefix — leading `/`, no trailing `/` — because that is what a host prefix-matches request paths against.

The annotated function stays callable. The same router that crosses the ABI can be driven in-process by an ordinary test, with no host in the loop:

```rust
let response = build().call(&request_frame).unwrap();
```

A complete example, built as a real shared library on every CI run, is in [`examples/repe_plugin.rs`](https://github.com/repe-org/repe-rs/blob/main/examples/repe_plugin.rs). CI then hands that library to a C++ Glaze host (`interop/cpp/plugin_host.cpp`) and drives it, so the ABI is pinned against the implementation that defines it rather than only against this crate's idea of it — see [C++ Interoperability](interop.md).

## The response-buffer contract

`repe_plugin_call` returns a `repe_buffer` — `{ const char* data; uint64_t size; }` — borrowed from a plugin-owned thread-local. It stays valid **only until that thread's next call**, so a host must copy before calling again. Nothing is freed across the heap boundary, which is what makes this an unusually clean FFI contract: the plugin owns every allocation it hands out, for its own lifetime.

Two details are easy to get wrong in a hand-written shim, and both are silent when wrong:

- **`size == 0` still carries a non-null `data`.** Glaze's helper returns `std::string::data()`, which is never null. A C++ host that builds a `std::string_view` from a null pointer with length 0 is undefined behavior even though it appears to work. This crate points at a static byte instead.
- **A notify answers with `size == 0`.** REPE notifies produce no response, and a host must read zero size as "send nothing" rather than as an error. This holds even when the handler panicked: an error frame in reply to a notify is a frame no client is awaiting.

Failures never cross the boundary as unwinding — including a call made from a `thread_local` destructor during thread teardown, when the plugin's own thread-local buffer may already be gone. A panicking handler, a malformed frame, an out-of-range `request_size`, and a call after shutdown all come back as REPE error responses with an `id` of `0`.

A panicking **constructor** is latched, not retried: every later call answers with the same error rather than running it again. `repe_plugin_init` is optional in this ABI, so a plugin whose constructor fails is otherwise re-entered once per request for the life of the host — repeating whatever it had already claimed (a device handle, a worker thread, a lock file) before it panicked.

One case a host controls: **do not pass a previous response back as the next request.** It aliases the buffer being written, and the plugin refuses it rather than serving it.

## Concurrency

`plugin.h` permits concurrent calls from multiple threads, each with its own response buffer, and this implementation honors that.

Whether the *work* runs concurrently depends on what the router holds. A router built with `with_struct` puts the value behind a `Mutex`, so every call against it — including a pure field read — serializes. Use `with_struct_rw` instead and reads share the guard:

```rust
use repe::server::Router;

# #[derive(Default, serde::Serialize, serde::Deserialize, repe::RepeStruct)]
# struct Instrument { gain: f64 }
#[repe::plugin(root = "/instrument")]
fn build() -> Router {
    Router::new().with_struct_rw("/instrument", Instrument::default()).0
}
```

That is what `examples/repe_plugin.rs` does, and it is the shape a plugin should ship: the ABI promises concurrent calls, and this is what makes the promise hold for reads. See [Reads share the guard](server.md#reads-share-the-guard) for exactly which endpoints qualify. Writes and `&mut self` methods still take the lock exclusively, so a handler that blocks on hardware still queues everything behind it — long-running work belongs on an owning worker thread with handlers that enqueue and return.

A panicking handler answers with an error response rather than a second panic, so the plugin degrades instead of taking the host down with it. How far it degrades depends on which guard the handler held, and the difference is worth stating plainly:

- A **`&mut self`** handler (or any handler under a `Mutex`) poisons the lock. `std` never clears poison, so every later request under that root, including a read of an unrelated field, answers with an error for the life of the host process. One panicking method retires the whole object, and there is no recovery short of restarting the host.
- A **`&self`** handler under an `RwLock` does not. `std::sync::RwLock` poisons only on a panic while the write guard is held, so a panicking read leaves the object serving.

A plugin that must survive a `&mut self` handler bug should publish its state through individual handlers rather than a registered struct.

## Deployment requirements

Each of these fails in a way that is hard to trace back to its cause.

- **Build with `panic = "unwind"`.** The panic guard in `repe_plugin_call` is inert under `panic = "abort"` — routinely set on embedded targets to shrink binaries — and any handler bug then takes the whole host process down.
- **Hosts should not `dlclose` a Rust plugin.** The response buffer is a thread-local, and on glibc unloading a library while threads that touched its TLS are still alive leaves destructor addresses pointing into unmapped memory. The crash lands at thread exit, arbitrarily far from the unload. A host that hot-reloads plugins should leak the handle.
- **One plugin per `cdylib`.** `dlsym` resolves by name, so the exports are global. Invoking the macro twice in one library is a duplicate symbol error at link time.

## ABI versioning

`REPE_PLUGIN_INTERFACE_VERSION` is currently `3`. A host must check it before reading anything else, including the metadata struct, since the version is what says whether that struct's layout can be trusted.

Pinning the constant here is deliberate coupling: the ABI belongs to the protocol rather than to either implementation, so a bump on the Glaze side needs a repe release before any Rust host or plugin speaks the new version. That is the same coupling already accepted for [the wire format](interop.md), managed the same way.

Glaze's header recommends an exact-equality check. `Plugin::load` uses a **closed range** instead, because the two ends are not symmetric: a plugin must be exact, since it exports whatever it was compiled against, but a host can reasonably drive an older plugin — and the alternative is that every ABI bump orphans every plugin binary in a deployment on the same day. `plugin::host::supported_interface_versions()` is that range. It is `3..=3` today, since 3 is the only layout this crate has ever bound; what it buys is the *next* bump, which if additive leaves already-deployed plugin binaries loading. A deployment that wants exact equality can have it by comparing `plugin.interface_version()` after loading.

## Beyond the router

`#[repe::plugin]` is a thin wrapper over `plugin::PluginRuntime`, which holds the router and the lifecycle state. Write the five exports by hand against it when the generated shape does not fit — a plugin that must consult its environment before choosing a router, for instance. The buffer contract and the panic guard come from the runtime either way.

## Hosting plugins

Requires the `plugin-host` feature. `Plugin::load` resolves the symbols, performs the version handshake **before** reading anything the version governs, reads the metadata, and runs the optional initializer:

```rust
use repe::plugin::host::Plugin;

// SAFETY: loading a native library runs its initializers, so the caller
// vouches for the file. Everything after that, the crate checks.
let plugin = unsafe { Plugin::load("libinstrument.so") }?;
println!("{} {} claims {}", plugin.name(), plugin.version(), plugin.root_path());

if let Some(response) = plugin.call(&request_frame)? {
    // Owned. Nothing borrows across the boundary.
}
```

`call` returns `Ok(None)` for a response of size zero, which is what a notify produces and which a host must read as *send nothing* rather than as an error. A request the plugin rejects — an unknown method, a malformed frame, a handler that failed — comes back as `Ok(Some(frame))` carrying a REPE error response: that belongs to whoever sent the request, and the host forwards it rather than interpreting it. `HostError` is reserved for a plugin that failed to hold up its end of `plugin.h`. `call_into` writes into a caller-owned buffer for a host that keeps one per connection or per worker.

`Plugin` is `Send + Sync`, and holds no borrows: the metadata is copied in at load and each response is copied out before it is returned. Sharing one across a thread pool is the intended use, and it is the copy that makes it safe — the plugin's buffer is a thread-local that dies at that thread's next call.

### Routing

A host dispatches with `Plugin::claims`, against a query that `MessageView` reads out of a frame without decoding the body:

```rust
let query = MessageView::from_slice(&frame)?.query_str()?;
if let Some(plugin) = plugins.iter().find(|p| p.claims(query)) {
    // ...
}
```

Use it rather than `query.starts_with(p.root_path())`: the separator has to be part of the comparison, or a plugin rooted at `/inst` swallows every request meant for one rooted at `/instrument`. That works in every test a host writes and fails once both are deployed.

`load` refuses a `root_path` that is relative or carries a trailing separator, because either one matches nothing and does it silently: the plugin loads, reports healthy, and every request under it comes back method-not-found. An **empty** root is accepted — it is what Glaze's registry reports for an object published at the top level — and claims every absolute query.

### Mounting a plugin on a router

`Plugin` implements `HandlerErased`, so a plugin can be served straight from a `Router` — the frame marshalling between the router's decoded request and the ABI's byte buffers is the crate's, not the application's:

```rust
use repe::plugin::host::Plugin;
use repe::server::Router;
use std::sync::Arc;

// SAFETY: loading a native library runs its initializers.
let plugin = Arc::new(unsafe { Plugin::load("libinstrument.so") }?);
let router = Router::new()
    .with_json("/host/version", |_| Ok(serde_json::json!(env!("CARGO_PKG_VERSION"))))
    .with_fallback_blocking(plugin.clone());
```

`Router::with_fallback` registers a handler for requests no route claims, and it is resolved **last** — after the fixed routes, the mounted registries, and the mounted structs — so nothing static is slowed down by it and a host route always wins. That is what makes it the mount point for a table that does not exist yet when the router is built: plugins loaded on demand, a proxy to another node, a registry mounted after startup.

Use the `_blocking` variant for a plugin, as above: `repe_plugin_call` enters a library this process did not build, for a time nothing here bounds, so on the WebSocket server it belongs off the reader task. On the TCP servers the two are identical.

Mounted as the fallback, a single `Plugin` answers what `claims` matches and frames `MethodNotFound` for the rest, since by then nobody else is left to serve it. For several plugins the fallback is the host's own handler: it owns the table, consults `claims`, and calls through to the plugin it picked. Which plugins are in that table, and when, stays the application's — see [What is deliberately not here](#what-is-deliberately-not-here).

A plugin's own error frames pass through unchanged — an unknown method under its root, a handler that failed, a body it rejected. Only a plugin that breaks the ABI — answering a call with nothing, or with a frame that does not parse — is turned into an `InternalError` response, so the caller gets an answer either way.

One thing a mount does **not** cover: a registry or struct mounted at a prefix answers for that whole prefix, misses included. If a mounted struct sits at `/instrument` and a plugin claims the same root, the struct wins every path under it and the plugin is never reached. Give them separate roots.

`shutdown` consumes the handle, so keep the typed `Arc<Plugin>` as above and hand the router a clone: `Arc::try_unwrap` does not apply to the `Arc<dyn HandlerErased>` the router holds, because `dyn HandlerErased` is not `Sized`.

```rust
drop(router);
Arc::try_unwrap(plugin).expect("no other holders").shutdown();
```

A plugin meant to live as long as the process needs none of that.

### The library is never unloaded

`load` leaks its handle, and dropping a `Plugin` unloads nothing. That is the only correct behavior available — see the `dlclose` bullet above — and it has consequences worth planning for rather than discovering:

- **Reloading a path yields the resident copy, not the new file.** `dlopen` refcounts by path, so replacing a plugin binary in place and reloading it serves the old code with no error anywhere. Publish each build under its own path if reload has to mean anything.
- **Two `Plugin` values for one path are two handles to one instance.** They share the plugin's state, its initialization, and its shutdown. A second `load` reaching an already-initialized plugin is treated as success, not as a conflict.
- **`shutdown` is one-way and belongs to the library, not the handle.** It consumes the `Plugin`, but reloading the path afterward reaches the same retired instance — for a plugin built on this crate, `load` then fails with `HostError::InitFailed`. Dropping a `Plugin` deliberately does *not* shut it down, or an ordinary early return would retire the plugin for the rest of the process.

### What is deliberately not here

The loader and the call wrapper are generic. Everything above them is deployment policy rather than protocol, and is left to the application: which directory plugins are read from, which file extensions count, whether a reload probes before replacing, and whether any of that is published as an RPC surface. Those answers differ per deployment.

A worked host is in [`examples/plugin_host.rs`](https://github.com/repe-org/repe-rs/blob/main/examples/plugin_host.rs). It takes a library path on argv and drives whatever is there, so the same binary loads a Rust plugin and a C++ Glaze one — which is what CI uses it for, in both directions. See [C++ Interoperability](interop.md).

One thing a host cannot check from the outside: a panic or an exception that escapes `repe_plugin_call` unwinds through an `extern "C"` frame and aborts. The guard is the plugin's, and so is the build setting that keeps it live.

## Transport-free dispatch

Plugins are built on `Router::call`, which is public and useful on its own:

```rust
let response: Option<Vec<u8>> = router.call(&request_frame);
```

Bytes in, bytes out, no transport. It does the same routing work the built-in servers do between reading a frame and writing one back: version and query-format validation, handler resolution, notify semantics (`None` means *send nothing*), `MethodNotFound` framing, and the response query echo.

It differs from them in two ways, both because the caller hands over one complete frame rather than a stream. It requires exactly one frame per buffer, so trailing bytes are an error rather than a silently-served prefix; and an unparseable frame comes back as an id-0 error response rather than dropping the connection, matching what a plugin must return to its host.

Reach for it for any carrier this crate does not ship a server for — shared memory, a foreign event loop, an in-process test. `Router::call_into` writes into a caller-owned buffer instead of allocating, which is what a per-connection or per-thread buffer wants.
