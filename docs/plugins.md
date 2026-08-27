# Plugins

A REPE **plugin** is a shared library that a host loads at runtime and drives over a small C ABI. The payloads crossing the boundary are ordinary REPE frames, so a plugin is just a router that happens to live behind `dlopen` instead of behind a socket.

The ABI is defined by [Glaze](https://github.com/stephenberry/glaze) in `glaze/rpc/repe/plugin.h`, and this crate implements the same one. A `cdylib` built here loads into an existing C++ REPE host with no adapter on either side, which is the cheapest way to introduce Rust into an established C++ deployment: one leaf at a time, without touching the host.

## Building a plugin

Requires the `plugin` feature. The library must be a `cdylib` — that is what produces a `.so`/`.dylib`/`.dll` with resolvable symbols.

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
repe = { version = "8", features = ["plugin"] }
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

Whether the *work* runs concurrently depends on what the router holds. A router built with `with_struct` puts the value behind a `Mutex`, so every call against it — including a pure field read — serializes. A plugin whose handlers block on hardware, under a host running several threads, will queue unrelated reads behind them. Long-running work belongs on an owning worker thread with handlers that enqueue and return.

A panicking handler leaves that mutex poisoned, which surfaces as an error response rather than a second panic: the plugin degrades to answering errors instead of taking the host down with it. How far it degrades is worth stating plainly — `std` never clears poison, so every later request under that root, including a read of an unrelated field, answers with an error for the life of the host process. One panicking method retires the whole object, and there is no recovery short of restarting the host. A plugin that must survive a handler bug should publish its state through individual handlers rather than `with_struct`.

## Deployment requirements

Each of these fails in a way that is hard to trace back to its cause.

- **Build with `panic = "unwind"`.** The panic guard in `repe_plugin_call` is inert under `panic = "abort"` — routinely set on embedded targets to shrink binaries — and any handler bug then takes the whole host process down.
- **Hosts should not `dlclose` a Rust plugin.** The response buffer is a thread-local, and on glibc unloading a library while threads that touched its TLS are still alive leaves destructor addresses pointing into unmapped memory. The crash lands at thread exit, arbitrarily far from the unload. A host that hot-reloads plugins should leak the handle.
- **One plugin per `cdylib`.** `dlsym` resolves by name, so the exports are global. Invoking the macro twice in one library is a duplicate symbol error at link time.

## ABI versioning

`REPE_PLUGIN_INTERFACE_VERSION` is currently `3`. A host must check it before reading anything else, including the metadata struct, since the version is what says whether that struct's layout can be trusted.

Pinning the constant here is deliberate coupling: the ABI belongs to the protocol rather than to either implementation, so a bump on the Glaze side needs a repe release before any Rust host or plugin speaks the new version. That is the same coupling already accepted for [the wire format](interop.md), managed the same way.

Glaze's header recommends an exact-equality check. A host written against this crate should prefer a **closed range**, because the two ends are not symmetric: a plugin must be exact, since it exports whatever it was compiled against, but a host can reasonably drive an older plugin — and the alternative is that every ABI bump orphans every plugin binary in a deployment on the same day. No such predicate ships here, since no host does; it belongs with the host that needs it.

## Beyond the router

`#[repe::plugin]` is a thin wrapper over `plugin::PluginRuntime`, which holds the router and the lifecycle state. Write the five exports by hand against it when the generated shape does not fit — a plugin that must consult its environment before choosing a router, for instance. The buffer contract and the panic guard come from the runtime either way.

## Hosting plugins

The other direction — a Rust host that `dlopen`s plugins — is not in this crate yet. `interop/cpp/plugin_host.cpp` is a working C++ one, and is the clearest available reference for the sequence: resolve, check the version, read the metadata, route by `root_path` prefix, call, copy before the next call.
 The ABI binding and the buffer contract are here; the loader, its lifecycle, and the policy around it (plugin directories, hot reload, prefix matching) are not.

A host needs three layers, and only the first two generalize:

1. **ABI binding** — the types and symbol signatures. This crate, today.
2. **Host** — `dlopen`/`dlsym`, the version check, a safe call wrapper that copies the response before the borrow expires. Generic, and where all the unsafe lives.
3. **Manager policy** — plugin directory resolution, hot reload, extension filtering, prefix matching, the RPC surface that exposes all of it. Deployment policy, not protocol, and it belongs in the application.

Layer 2 is small but its shape is not yet known; freezing it into a published API before it has run against real plugins would ship the wrong one permanently. It is expected to be proven inside a consumer first and promoted here afterward.

## Transport-free dispatch

Plugins are built on `Router::call`, which is public and useful on its own:

```rust
let response: Option<Vec<u8>> = router.call(&request_frame);
```

Bytes in, bytes out, no transport. It does the same routing work the built-in servers do between reading a frame and writing one back: version and query-format validation, handler resolution, notify semantics (`None` means *send nothing*), `MethodNotFound` framing, and the response query echo.

It differs from them in two ways, both because the caller hands over one complete frame rather than a stream. It requires exactly one frame per buffer, so trailing bytes are an error rather than a silently-served prefix; and an unparseable frame comes back as an id-0 error response rather than dropping the connection, matching what a plugin must return to its host.

Reach for it for any carrier this crate does not ship a server for — shared memory, a foreign event loop, an in-process test. `Router::call_into` writes into a caller-owned buffer instead of allocating, which is what a per-connection or per-thread buffer wants.
