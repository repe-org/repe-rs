# Changelog

## [Unreleased]

Next release is a **major**: the shared-borrow trait methods change shape, and a whole-child write now descends.

### Added
- **New crate: `repe-core`.** The `RepeStruct` surface on its own — the trait, `RepeMethods`, `StructError`, `ResponseBody`, and the protocol constants those name. A crate that only *declares* a served type can now derive against it without pulling in the server, the client, or the transport, which a crate with a deliberately light dependency list could not do before. `repe` re-exports all of it at the paths it has always had (`repe::structs::*`, `repe::constants::*`), and `#[derive(RepeStruct)]` resolves against whichever of the two crates is in scope. Nothing moved for a caller of `repe`.

- **Struct-level `#[repe(readonly)]`.** Refuses a write of the whole object — and, because it emits only the refusal, never generates `serde_json::from_value::<Self>`. A derived struct is therefore no longer required to be `DeserializeOwned`, which a struct holding an open socket or a file handle never could be. Replaces a hand-written `Deserialize` that always errors.

- **`#[repe(listing_order("a", "b", ..))]`.** The whole-object listing's key order, named in full. Without it a `#[repe(get/set)]` endpoint is always last, so a `glz::object` with a `custom<setter, getter>` in the middle had no counterpart here. Naming the sequence in full makes a typo or an omission a compile error: at macro time for fields and listed methods, and by a `const` assertion for the impl block's endpoints, which the derive cannot see. Reorders emission only.

  It governs the two listings the router encodes through, which is every frame a client sees. It cannot govern `RepeStruct::repe_handle`, whose `serde_json::Map` sorts its keys unless the dependency graph enables `serde_json/preserve_order` — that has always been true of declaration order too.

- **`#[repe(nested_serde)]`.** Descend into a field that implements only `Serialize` + `DeserializeOwned`, by walking a `serde_json::Value` of it. For a type there is no way to annotate — a third-party one, or one whose crate must not take this dependency. Prefer `#[repe(nested)]` (with `repe-core` where the edge is the problem): it descends without materializing anything. The cost here is paid only on a sub-path.

- **`RepeStruct` for `Option<T>`.** A conditionally-present child. `Option` is foreign and the trait is not a host's, so this could only live here; the alternative was a newtype per crate. Present forwards to the inner value; absent reads as `null` at its own path and answers `MethodNotFound` to a write or any sub-path, because a silent no-op against a live resource is worse than an error. A `null` written at the child's own path is refused whether it is there or not: presence is the host's, not the client's, and honouring it would let a client remove a child it could never put back. Matches what Glaze publishes for an unmapped optional member.

### Changed
- **BREAKING: the shared borrow serves a `&self` method that takes arguments.** The read/write distinction was made per *frame*, so a call carrying a body took the exclusive guard however it was declared — and one long-running `&self` call stalled every read of the object for as long as it ran. That is a regression against the C++ registry this replaces, which has no mutex at all, and it silently defeated `with_struct_rw` for exactly the endpoints that need it. The receiver is known where the arms are generated, so the receiver now decides.

  `RepeStruct::repe_read_into` becomes `repe_shared_into(&self, segments, body: &mut Option<Value>, out)`, and `RepeMethods::repe_call_read_into` becomes `repe_call_shared_into` alongside it. Both still default to declining, so a hand-written impl that overrides neither is unaffected. The body is borrowed rather than moved so a decline leaves the exclusive retry the request it was handed, with no clone: take it only once the borrow has committed to answering.

  The JSON Pointer is split once for both attempts rather than once per attempt. A declining shared attempt used to hand the exclusive retry the same string to re-split, which for an escaped pointer meant running `json_pointer::parse` twice — four extra allocations per write, pinned now by `struct_write_under_a_shared_lock_splits_the_pointer_once`.

- **BREAKING: a whole-child write descends into the child.** `#[repe(nested)]` used to assign over the field — `self.child = from_value(body)?` — which was the one path where a child's own `RepeStruct` impl was never consulted, and exactly the path where a child that owns live state has something to say. It is now `Child::repe_handle_into(&[], Some(body))`, matching the read. A derived child's empty-segments arm is still `*self = from_value(..)`, so nothing that worked before changes; only a hand-written child gains a say.

  One consequence beyond the intent: a `#[repe(nested)]` child is no longer touched by `serde` from its parent at all — not on a write, not in the listing — so it needs neither `Serialize` nor `DeserializeOwned` for the parent's sake, only `RepeStruct`.

- **BREAKING: `#[repe(readonly)]` on a nested field refuses sub-path writes too.** It previously guarded only the whole-child write. The attribute says the field cannot be written, and a write below it mutates the field just as surely.

- **`ErrorCode`, `BodyFormat`, and `QueryFormat` now live in `repe-core`.** Re-exported unchanged; no caller-visible move. They are `#[non_exhaustive]`, which does nothing within a crate and everything across one, so `repe`'s own matches on them gained explicit unknown-variant arms. Eight report what the neighbouring unrecognized-code arm already reported; the ninth, `message::finish_response`, has no such neighbour and falls back to JSON, labelled as JSON.

- **`repe-derive` is now `0.5.0`.** Its generated code names items that only exist alongside this release — `repe_shared_into`, `assert_listing_order`, `serde_pointer` — so the two move together, as they did at 9.0.0.

- **New in `repe::structs`:** `serde_pointer` and `serde_pointer_set`, the pointer pair backing `#[repe(nested_serde)]` and useful on its own. `json_pointer::evaluate` is now a thin front end over `serde_pointer` rather than a second copy of the same walk. `assert_listing_order` and `listed_signature` are also new but `#[doc(hidden)]`: generated code names them, nothing else should, and `assert_no_endpoint_collision` is hidden with them for the same reason.

- **BREAKING: `StructError` is `#[non_exhaustive]`,** joining `ErrorCode`, `BodyFormat`, and `QueryFormat`. A match on it from outside the crate needs a wildcard arm; `code()` maps any variant onto its protocol code without one.

- **BREAKING: `structs::join_path` is removed.** It had no callers in this crate or its generated code, and 1.0.0 of `repe-core` is the wrong place to freeze a dead helper. `path_from_segments` is the one that is used.

- **`RepeStruct` carries an `on_unimplemented` note.** The likeliest way to hit it is `#[repe(nested)]` on a type that only implements serde, so the message names `#[repe(nested_serde)]` and the `repe-core` derive as the two answers.

- **Depend on `uniudp = "1.2.2"`** (raised from `1.2.1`) behind `fleet-udp`. 1.2.2 drops `mio`, `rand`, `subtle`, and `hmac`, taking 8 packages out of the lockfile: it moves to a plain `std::net::UdpSocket` and seeds from `getrandom` directly. The UDP wire format is unchanged and the MSRV floor is still 1.96 (below repe's 1.96.1), so this is a lockfile-and-floor move with no repe API change — repe drives only uniudp's sender.

  The `Cargo.toml` note explaining why `uniudp` is target-gated is corrected with it. The gate is still needed, but no longer because of mio: `getrandom` refuses `wasm32-unknown-unknown` unless the consumer enables its `wasm_js` backend. `--all-features --lib` still builds for wasm32.

## [9.1.0] - 2026-08-27

### Added
- **`Router::with_mount_fallthrough` — a mount's miss is still a miss.** A mounted registry or struct answers for its whole prefix, misses included, which is right at a prefix and degenerate at the root: a struct mounted at `""` matches every path, so it does not narrow a `with_fallback` handler, it makes it unreachable. With this on, a mount that would frame `MethodNotFound` hands the request to the fallback instead. Nothing is reordered, and registration order does not matter. Opt-in, because it replaces the mount's own diagnostic for paths it does not serve — and because the trigger is the error code, so a handler that deliberately answers `MethodNotFound` is superseded too.

- **`plugin::host::Plugin::load_origin`.** Whether the load mapped the library, reached a copy already resident, or could not be asked. `dlopen` refcounts by path, so reloading a resident path reads no file and runs no initializer; until now the two were indistinguishable, and a hot-reload endpoint reported success for a load that changed nothing. Answered with an `RTLD_NOLOAD` probe (`GetModuleHandleExW` on Windows); `LoadOrigin::Unknown` covers the four unix targets whose loader has no such flag. The ABI's `ALREADY_INITIALIZED` is still not this signal — a lazily-initializing plugin returns it on a genuine first load.

### Changed
- **A whole-object listing gives up the shared guard only when it has to.** It used to decline whenever the struct published any field-shaped endpoint, so one `#[repe(get)]` cost the shared listing of that struct and, transitively, of every struct nesting it. The listing must still decide before it invokes anything — a decline found partway through would run the getters before it twice — but the question is now whether any getter takes `&mut self`, asked across the whole subtree. A struct whose computed values are pure reads keeps its shared listing, with each accessor's value in it, and so does every ancestor. Two defaulted overridables carry it: `RepeMethods::REPE_LISTING_NEEDS_EXCLUSIVE` for one method table's accessors, and `RepeStruct::repe_listing_declines` for the subtree a parent composes. No frame changes.

## [9.0.0] - 2026-08-27

### Added
- **`Router::call` / `Router::call_into` — transport-free dispatch.** One serialized REPE request frame in, one response frame out, with no socket involved. This is the work the built-in servers do between reading a frame and writing one back: version and query-format validation, handler resolution, notify semantics (`None` means *send nothing*), `MethodNotFound` framing, and the response query echo. Reaching it through `Router::get` plus `HandlerErased::handle` meant reimplementing all five per carrier. `call_into` writes into a caller-owned buffer so a carrier holding one buffer per connection or per thread allocates nothing per request.

- **C-ABI plugin surface (`plugin` feature).** `#[repe::plugin(root = "/x")]` on a `Router` constructor exports it as a REPE plugin: the five symbols a host resolves after `dlopen`, against the same ABI Glaze defines in `glaze/rpc/repe/plugin.h`. A `cdylib` built here loads into an existing C++ REPE host with no adapter on either side. `name` and `version` default to the crate's `CARGO_PKG_NAME` / `CARGO_PKG_VERSION`; the annotated function stays callable, so the same router can be driven by an in-process test.

  The crate owns the three things a hand-written shim reliably gets wrong: the version handshake, the thread-local response buffer's borrow contract (including a non-null pointer at `size == 0`, which a C++ host's `string_view` requires), and a panic guard, since unwinding across the boundary would abort the host. A panicking handler, a malformed frame, a call after shutdown, and a reentrant call on one thread all come back as REPE error responses instead — except where the request was a notify, which is answered with nothing even when its handler panicked. A constructor that panics is latched rather than retried, so it cannot repeat a partial side effect once per request.

  `plugin::PluginRuntime` is the same machinery for a plugin that needs to write the exports by hand.

- **Plugin host (`plugin-host` feature).** The other direction: `plugin::host::Plugin` loads a plugin and drives it, so a Rust host runs existing C++ plugins unchanged. It resolves the symbols, checks the ABI version *before* reading anything whose layout the version governs, reads the metadata, and runs the optional initializer — treating both lifecycle symbols as absent-is-fine, which the header allows.

  `call` hands back an owned `Vec<u8>`, copying inside the call, which is what the borrowed response buffer requires and what makes `Plugin` `Send + Sync`. `Ok(None)` is a notify's zero-size response, not an error; a plugin's own error frames are forwarded to whoever sent the request rather than raised as host errors. `call_into` reuses a caller-owned buffer.

  The version check is a closed range rather than the exact equality Glaze's header recommends: a plugin exports whatever it was compiled against, but a host can reasonably drive an older one, and exact equality orphans every deployed plugin binary on the day of an ABI bump.

  `Plugin::claims` is how a host decides a request belongs to a plugin. It is not `query.starts_with(root_path)`, which lets a plugin rooted at `/inst` swallow every request meant for one rooted at `/instrument` — a bug that works in every test a host writes and appears once both are deployed.

  The library is deliberately never unloaded — unloading one whose thread-locals have been touched leaves TLS destructors dangling — so reloading a path reaches the resident copy, two handles to one path share one instance, and `shutdown` retires that instance for the life of the process. Deployment policy above the loader (plugin directories, hot reload, prefix matching, an RPC surface for any of it) stays with the application. `docs/plugins.md` has the details; `examples/plugin_host.rs` is a worked host.

- **`Router::with_fallback` — a handler for routes that do not exist yet.** Every registrar runs before `Server::serve`, so a path discovered at run time had nowhere to go, and there was no workaround: middleware is attached per route, so on a miss no pipeline exists and none of it runs. A fallback is resolved last — after the fixed routes, the mounted registries, and the mounted structs — so it is free on the hit path, and it *is* wrapped in the middleware pipeline. Nothing else answers a miss it is given, so a handler that declines a request frames `MethodNotFound` itself.

  One limit worth knowing: a registry or struct mounted at a prefix answers for that whole prefix, misses included, so a fallback sees only paths no mount covers. `with_fallback_blocking` is the off-reader variant, for a miss handler that leaves the process.

- **`plugin::host::Plugin` implements `HandlerErased`.** A loaded plugin mounts on a router in one line (`Router::new().with_fallback_blocking(plugin.clone())`), with the frame marshalling between the router's decoded request and the ABI's byte buffers handled by the crate. The plugin's own error frames pass through unchanged; only a plugin that breaks the ABI — answering with nothing, or with a frame that does not parse — becomes an `InternalError` response, so the caller is answered either way. Which plugins are in the table, and when, stays the application's.

- **Reads share the guard.** Struct dispatch took an exclusive lock for every request, so a `/version` read queued behind whatever long-running call held the object, and swapping in an `RwLock` changed nothing. A bodiless request — a read, by REPE's own frame-level distinction — now goes through `&self` where the derive can reach the value that way: every field at any nesting depth, a `#[repe::methods]` method taking `&self` and no arguments, the getter half of a field-shaped endpoint with a `&self` getter, and the whole-object listing when nothing in it has to be *invoked*. Everything else declines and is served exclusively as before, identically and invisibly.

  `Router::with_struct_rw` is `with_struct` behind an `RwLock`, which is the registration that turns any of this on; `with_struct` and its `Mutex` are unchanged, and there the shared path is compiled out rather than taking the same lock twice.

  A listing declines outright when the struct publishes any field-shaped endpoint. It is the one read that composes many others, so a decline discovered partway through would leave the getters before it called twice — once here and once on the exclusive retry — and a `&self` getter over a read counter would report the second call. Individual reads of those endpoints are unaffected.

  This is what makes a plugin honor the concurrency its own ABI advertises, so `examples/repe_plugin.rs` now registers its object that way.

  Three additions carry it, all defaulted: `Lockable::with_read` (`None` unless the lock has a shared mode), `RepeStruct::repe_read_into`, and `RepeMethods::repe_call_read_into`. A hand-written impl that overrides none of them behaves exactly as it did.

  Two consequences worth knowing. Two `&self` methods on one object now run at the same time, where the exclusive guard used to give them mutual exclusion for free. And a panic under a shared guard no longer poisons the lock, so a panicking `&self` handler no longer retires the object for the life of the process; a `&mut self` one still does.

  **BREAKING**, in one narrow way: a `#[repe(methods(..))]` entry declared `&self` is now *called* through a shared borrow, so declaring a `&mut self` method as `&self` is a compile error rather than a lie the listing repeated.

- **Field-shaped endpoints: `#[repe(get = "...")]` / `#[repe(set = "...")]`.** One endpoint served by a getter/setter pair, for a value that reads and writes like a field but is computed — a register in different units, a value derived from two others. It behaves as a field on the wire, listing its **value** in the whole-object read rather than a signature string, where publishing the same thing as methods would have changed the path. A getter with no setter is read-only, so a pair of real getters and no-op setters is no longer how a read-only computed value is spelled. `#[repe(typed)]` composes on the getter, either half may be fallible, and a setter with no getter is a compile error rather than an endpoint the listing cannot show.

- **Plugin-ABI interop coverage, both directions.** `interop/cpp/plugin_host.cpp` is a C++ Glaze host that `dlopen`s the Rust plugin example; `interop/cpp/example_plugin.cpp` is a C++ Glaze plugin that the Rust host `dlopen`s. Both run in the existing `interop` CI job against the same pinned Glaze tag as the wire-format fixtures. One direction alone would leave one implementation agreeing with itself, which it does by construction even when both of its ends misread the same clause of the header.

  Between them they pin the symbol table, `repe_plugin_data`'s layout read from each language's own declaration, the version handshake, the response-buffer contract (including a `std::string_view` built from a zero-size response), field reads and writes, methods with and without arguments, a handler `Err`, a `#[repe(typed)]` field crossing as BEVE, notify, `method_not_found`, a malformed frame, and post-shutdown refusal.

- **REST gateway (`rest` feature).** `RestGateway` fronts a `Registry` with HTTP/1.1 and HTTP/2, adding a front door rather than replacing one: public clients get curl, OpenAPI, and edge caching, while REPE clients keep the aligned numeric fast path and notify against the same registry.

  The translation is mechanical because both sides address state by JSON Pointer, and the registry's read/write/call trichotomy already matches the three HTTP verbs worth having: `GET` reads, `PUT` writes, `POST` calls. The mismatch is refused rather than coerced — `PUT` at a function and `POST` at a value are both `405` — which is what keeps the facade from becoming RPC in a REST costume with none of REST's guarantees. `OPTIONS` reports the `Allow` set the target actually supports.

  Reads carry a strong `ETag` (FNV-1a/64, so instances behind one load balancer agree) plus `Vary: Accept` and a configurable `Cache-Control`, so a conditional `GET` costs a `304` with no body. Bodies negotiate JSON against BEVE on both legs. Failures answer as RFC 9457 problem details carrying the originating REPE error code.

  Safety defaults assume an unauthenticated deployment: `read_only` and `allow_root_write` are both off unless asked for. `serve` caps concurrent connections and bounds each request with a timeout, so idle-connection floods and half-sent bodies cannot walk the process to its descriptor limit.

  `If-Match` is honored on writes (strong comparison, `412` on failure), so the validators reads hand out are usable for optimistic concurrency. Error responses carry `Cache-Control: no-store`, since RFC 9111 makes 404 and 405 heuristically cacheable and a cached 405 outlives the `Allow` it advertised. `OPTIONS *` reports the server-wide method set.

  `RestGateway::respond` is the whole mapping with no transport involved — the REST-side counterpart to `Router::call` — so `serve` is a thin hyper shim over it and any other HTTP stack can be one too.

- **`Registry::write_if` / `Registry::call`.** A conditional write whose comparison and write are one critical section, and a dispatch that is committed to calling a function rather than re-deciding from the body. `dispatch` gives neither: evaluating a validator with a separate read and then writing is check-then-act, and a caller that already resolved "this is a value" races anyone registering a function at that pointer in between — losing by handing its write payload to a function as arguments.

- **`Registry::is_function`.** Whether a pointer names a registered function rather than a value. The registry decides read-vs-write-vs-call from the body alone, which is right for REPE; a caller that must commit to a verb before it has a body needs the distinction up front, and probing it with a read is not a substitute because a read of a function returns a descriptor a stored value could equally well contain.

### Changed
- **BREAKING: `beve` moves to 9.** As in every previous beve major, beve types appear in repe's public API (`RepeError::Beve(beve::Error)`, the re-exported `beve::{BeveTypedSlice, Complex}`, and the `T: beve::BeveTypedSlice` bounds on the typed/complex surfaces), so a beve major is a repe major; downstreams that also depend on `beve` directly must move to `beve 9`. MSRV is unchanged at 1.96.1. It is a security release: the decoders had no recursion limit, and nesting is declared by the input rather than by the destination type, so a few KB of nested array tags recursed until the thread stack was gone. That is not an ordinary parse failure — a Rust stack overflow *aborts* rather than unwinding, so no `Result` carried it and the per-connection `catch_unwind` in the servers could not contain it. One anonymous request took down every other connection the process was serving. Input nested past `beve::MAX_RECURSION_DEPTH` (128) is now refused, and repe answers it with `ErrorCode::ParseError` like any other malformed body.

  `beve::Error` also becomes `#[non_exhaustive]` in that release. repe only wraps it (`RepeError::Beve`), so no repe signature changes, but a downstream crate matching `beve::Error` exhaustively needs a wildcard arm.

  Nothing else in beve 9 reaches repe: the `mat` feature it also moves is not enabled here.

- **`RestConfig::accept_beve_bodies` now defaults to `true`.** Its entire reason for being off was the decoder above, which is fixed; a nested body is now a `400` rather than a dead process. The knob stays, because refusing a media type you do not document is a legitimate policy, but it is no longer a safety default.

- **BREAKING: `structs::assert_no_endpoint_collision` takes a third argument.** It now checks all three endpoint sets on a struct against each other — what the derive publishes, `REPE_METHOD_SIGNATURES`, and the new `REPE_ACCESSOR_ENDPOINTS` — and the derive passes struct-level `#[repe(methods(..))]` endpoints into the first, which it previously omitted. The function exists to be called from macro expansion; regenerating with the matching derive is the whole migration.

- **`repe-derive` is now `0.3.0`**, and `repe` requires it. The macro crate gained field-shaped endpoints and the shared read path, and its generated code names items that only exist in `repe` 9.0.0, so the two move together. Nothing to do unless you depend on `repe-derive` directly, which is not the supported way to reach it — use `repe::RepeStruct` / `repe::methods`.

### Fixed
- **A struct-level `#[repe(methods(..))]` endpoint could shadow an impl-block one silently.** The cross-macro collision check only saw *field* names, so a method listed on the struct and a method of the same name in the `#[repe::methods]` block both compiled: one became unreachable and the whole-object listing emitted the key twice — as two different values, since a `serde_json::Map` deduplicates the key and the streaming encoder does not. Both are now rejected at compile time.

- **The derive macros now work in this crate's own examples and doc-tests.** `proc_macro_crate` reports `FoundCrate::Itself` for every target that is not an integration test, so the generated paths were `crate::…` — correct for the library, wrong inside an example, where `crate` is the example. The macros now emit `::repe`, which `extern crate self as repe;` in `lib.rs` makes resolve from inside the library too. No effect on downstream crates, which were already on the `::repe` path.

- **A 48-byte frame could panic the parser (security).** `query_length` and `body_length` are attacker-controlled `u64`s read off the wire, and the framed total `48 + query_length + body_length` was summed unchecked. With `query_length = u64::MAX - 47` the sum wraps to `0`, which the header's own `length` field can be set to match — so the frame decoded, and slicing the query out of it computed `buf[48..0]` and panicked.

  Neither the TCP nor the async server catches unwinds, so one such frame took down the connection thread; under `panic = "abort"` it takes the process. `Header::decode` now rejects a total that does not fit both `u64` and the target's address space, with the new `RepeError::FrameLengthOverflow`. That single check is what makes every later `query_length as usize` in the crate safe, including on 32-bit targets such as `wasm32`.

## [8.1.0] - 2026-08-25

### Added
- **`#[repe::methods]` — methods reflected off the impl block.** An attribute macro on an inherent `impl` that publishes every method in it, deriving names, arities and types from the signatures. Adding a method now adds the endpoint; the struct-level `#[repe(methods(..))]` list could only be kept in step by hand, and *adding* to it was the one drift the compiler could not catch.

  The struct opts in with a bare `#[repe(methods)]` beside its `#[derive(RepeStruct)]`. A derive cannot see `impl` blocks, so the two halves are tied together at compile time: `structs::MethodsDeclared` (which the derive emits) is a supertrait of the generated `structs::RepeMethods`, so leaving either half off fails the build naming the attribute that is missing. Associated functions (`fn new() -> Self`) are not endpoints; `#[repe(skip)]` and `#[repe(rename = "...")]` cover the rest. `self`-by-value, `async`, `unsafe`, generic, reference-argument and `#[cfg]`-gated methods are compile errors rather than silent omissions — `#[cfg]` because conditional compilation runs *after* attribute macros, so the endpoint would be published for a method that may not exist.

  Two declarations claiming the same endpoint — two fields, two methods, or a field and a method — are rejected. Otherwise one silently wins dispatch, the other is unreachable forever, and the whole-object listing carries the key twice.

  The list form stays as the escape hatch for a block that cannot be annotated (a foreign type, an impl behind another macro), and the two may be used together.

- **Methods take any number of arguments**, on both surfaces — the previous cap was one, undocumented, and surfaced as a macro error. Zero arguments ignore the body, one *is* the body (unchanged on the wire), and two or more arrive as a positional array or an object keyed by parameter name.

- **`Result` returns map to error frames.** A method returning `Result<T, E>` sends `Ok(v)` as the payload and turns `Err(e)` into an error frame carrying `e.to_string()`, instead of serializing the `Result` itself.

  Detection is **name-based** — a macro sees a type, not a resolved one — so it covers `Result<T, E>`, `std::result::Result<T, E>` and the one-parameter aliases (`anyhow::Result<T>`, `std::io::Result<T>`, a crate's own `pub type Result<T>`). A `Result` aliased under another name (`type DeviceResult<T> = ...`) is **not** recognized and is serialized as data, so `Err` reaches the client as a success frame carrying `{"Err": ...}`. Spell such a return type as `Result<..>` on any method you publish; both boundaries are documented in `docs/server.md` and pinned by tests.

- **`#[repe(typed)]` field attribute**, routing a numeric array or `Vec` field to the bulk BEVE typed-array encoder (one `copy_nonoverlapping`, byte-identical to Glaze) rather than a per-element serde walk. The response carries `BodyFormat::Beve`; writes are unaffected and still take JSON. Inside the whole-object read the field stays a JSON array, since that frame is already JSON.

- **`RepeStruct::repe_handle_into`**, plus `structs::{ResponseBody, ObjectBody, RepeMethods, MethodsDeclared, MethodArgs}`. A provided method with a default that delegates to `repe_handle`, so existing hand-written impls are unaffected.

### Changed
- **Struct reads no longer build an intermediate `serde_json::Value`.** The router now dispatches through `repe_handle_into`, which serializes the live field straight into the outgoing frame buffer. Reading a four-field status block measured 1 allocation in place against 9 through `Value`; end to end — request query included — every struct read is now 2 allocations, leaf or whole-object. Pinned by `struct_read_allocation_budget` and `encoding_in_place_beats_the_value_path` in `tests/allocations.rs`.

- **`repe-derive` is now `0.2.0`**, and `repe` requires it. The macro crate gained the `methods` attribute and its generated code names items that only exist in `repe` 8.1.0, so the two move together. Nothing to do unless you depend on `repe-derive` directly, which is not the supported way to reach it — use `repe::RepeStruct` / `repe::methods`.

- **A whole-struct read emits its keys in declaration order**, where it previously inherited `serde_json::Map`'s ordering — alphabetical, unless something in the dependency graph enabled `serde_json/preserve_order`. Declaration order is stable regardless and is what Glaze emits. Object key order is not semantic, so a conforming client is unaffected; anything comparing struct-listing bodies byte-for-byte is not.

## [8.0.1] - 2026-08-17

### Fixed
- **`value-stream` and `fleet-udp` no longer break a `wasm32-unknown-unknown` build.** Both features' modules have always been `not(target_arch = "wasm32")` in `lib.rs`, but their non-portable dependencies sat in the untargeted `[dependencies]` table, so Cargo built them for wasm32 regardless — for a consumer that `cfg` had already compiled out. `zstd` failed inside `zstd-sys`'s build script (clang: "No available targets are compatible with triple wasm32-unknown-unknown", compiling `huf_decompress_amd64.S`) and `uniudp` failed inside mio ("This wasm target is unsupported by mio"). Both are now declared under `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` alongside `tokio`, so enabling either feature on wasm32 is inert rather than fatal.

  The cost landed on workspaces rather than on single crates. Cargo features are purely additive: a crate inheriting a workspace dependency can add features but never subtract one. A workspace that hoists one shared repe pin — the usual way to make repe compile once and share a single `beve` build across several products — therefore could not carry `value-stream` at all if any member targeted wasm32, even when that member wanted nothing but core `Message` framing. The workaround was a second, hand-maintained repe entry with `default-features = false` for the browser crate, and a comment explaining why the two must not be merged.

  Native builds are unaffected in every respect: the same dependencies resolve, with the same features, the same API, and the same wire output, and `Cargo.lock` does not move. The gate is `target_arch`, matching the existing module gates exactly, so `wasm32-wasip1` and `wasm32-wasip2` likewise stop building a dependency whose only consumer was already `cfg`'d out there.

- **`CallContext::with_cancel` and `stamp_response_query` no longer warn as dead code on wasm32.** Their `#[cfg_attr(..., allow(dead_code))]` predicates keyed on `not(feature = "websocket")`, but their only caller — `websocket_server` — is gated on that feature *and* `not(target_arch = "wasm32")`. With `websocket` enabled on wasm32 the allow went inactive while the caller stayed absent, so the build warned about functions it had itself removed the users of. Both predicates now mirror the module's own gate. Surfaced by the new CI step below, and unreachable before it, since nothing built that combination.

### Changed
- **CI clippies `--all-features --lib` for `wasm32-unknown-unknown` at `-D warnings`.** The job's previous comment said `--all-features` was "wrong here on purpose" because `value-stream` pulls zstd. That was true when written, and it is also why the target-gating defect went unnoticed: the failure was in a build script no job ever reached, so the manifest and the `cfg`s in `lib.rs` were free to disagree. `--lib` excludes the `cli` binary, which is a native CLI (tokio runtime, sockets) and is not meant to target wasm32 at any feature set; every other feature is now covered here automatically as more are added.

- `value-stream` appears in the feature tables in `README.md` and `docs/index.md`, which had omitted it entirely, together with a note that it and `fleet-udp` compile away on wasm32 instead of failing.

## [8.0.0] - 2026-08-17

### Changed
- **BREAKING:** upgraded the `beve` dependency from `7` to `8`. As in the 7.0.0 and 4.0.0 bumps, beve types appear in repe's public API (`RepeError::Beve(beve::Error)`, the re-exported `beve::{BeveTypedSlice, Complex}`, and the `T: beve::BeveTypedSlice` bounds on the typed/complex body, route, and stream-pull surfaces), so a beve major is a repe major. Downstreams that also depend on `beve` directly must move to `beve 8`.

  **No repe API change and no wire change**, and nothing in the suite needed editing. beve 8 exists for one reason: its `beve::complex::*_array` and `beve::complex_array::*` serde helpers took an unconstrained `T` and checked it with `size_of`/`align_of` at run time, which let a padded or wrong-class type through and read uninitialized bytes into the output. They now require the new unsafe `beve::ComplexElement` trait. repe does not reach complex arrays that way: every complex path here — `MessageBuilder::body_complex_slice`, `Message::decode_complex_slice`, `write_message_complex_slice`, and the SVS bulk complex modes — calls `to_writer_complex_slice` / `complex_slice_size` / `read_complex_slice` / `read_complex_slice_from_reader`, which take `&[beve::Complex<T>]` concretely and were never generic over a caller's own type. Those signatures are untouched.

  It can still reach an **application** payload struct, since that struct is compiled against beve directly: a field annotated `#[serde(with = "beve::complex_array::...")]` or `serialize_with = "beve::complex::f32_array"` over a hand-rolled complex type now needs `unsafe impl beve::ComplexElement for MyIq { type Component = f32; }`. `beve::Complex<T>` and (behind beve's new `num-complex` feature) `num_complex::Complex<T>` work unchanged.

- **MSRV raised to 1.96.1** (from 1.96), the floor beve 8 declares. The previous 1.96 floor came from `uniudp` and only bound the optional `fleet-udp` feature; this one binds every build, since `beve` is not optional. The full `--all-features --all-targets` build checks clean on 1.96.1; on 1.96.0 cargo now refuses to resolve.

## [7.2.0] - 2026-08-17

### Added
- **`SharedWebSocketServer::adopt_upgraded(io)`.** Serves a connection whose upgrade repe did not perform, wrapping the already-upgraded byte stream into a server-role `WebSocketStream` that carries this server's configured `WebSocketLimits`.

  The shipped one-port recipe (`is_websocket_upgrade` + `WebSocketServer::accept`) requires repe to own the `TcpStream` before any HTTP is parsed. That excluded the shape an embedder actually has when its routes live in an HTTP framework: an `axum` `Router` already holding `/healthz` and some `/api/*` routes, wanting the REPE endpoint to be one more route on it. There, the framework owns the socket and answers the `101`, so repe never sees a `TcpStream` — and no entry point accepted an already-upgraded stream, at any price. The only ways out were a second port or abandoning the framework's serving path for a hand-written accept loop. See [docs/websocket.md](docs/websocket.md#adopting-a-connection-your-http-framework-upgraded) for the worked `axum` recipe.

  Path validation is the caller's job here (the framework already routed the request), which is the counterpart to the `path` argument `WebSocketServer::accept` takes.

- **`SharedWebSocketServer::adopt_upgraded_partially_read(io, buffered)`**, for a framework that returns the bytes it read past the upgrade request separately from the stream. A client may legally pipeline its first frame with the upgrade, so dropping those bytes loses a frame; `hyper` replays them itself, but a framework handing back `(io, buffered)` has nowhere else to put them.

- **`HandshakeContext::from_http_request(&req)`.** `HandshakeContext` had no public constructor, so `on_peer_connect_with_handshake` — the hook for keying peers off an identity carried in the upgrade request — could not fire on a connection repe did not accept. That excluded the best-positioned caller in the crate: a framework handler holding the entire request. Generic over the body type and borrowing, so it composes with any `http::Request` and leaves the caller free to answer the upgrade afterwards.

- **`derive_accept_key`** (at the crate root): the `Sec-WebSocket-Accept` derivation an embedder answering the upgrade itself needs. It is the one part of the handshake that is easy to get subtly wrong, and a wrong answer is rejected by the client, not by this crate. A thin wrapper rather than a re-export of the transport's function, so the signature is repe's to keep stable.

- **`repe::tokio_tungstenite`**, the `tokio-tungstenite` this crate is built against. Needed only to *name* `WebSocketStream` (or the `http` types behind `from_http_request`) in an embedder's own signatures — the value comes from `adopt_upgraded`, which infers it. Previously an embedder had to guess which version requirement would unify with repe's.

### Changed
- `SharedWebSocketServer::serve_connection`, `serve_connection_with_handshake`, `serve_connection_with_cancel`, and `serve_connection_with_cancel_and_handshake` are now generic over the underlying byte stream (`S: AsyncRead + AsyncWrite + Unpin + Send + 'static`) rather than fixed to `WebSocketStream<TcpStream>` — matching `proxy_connection`, which was already generic. The connection loop never used a `TcpStream` method, so the restriction was incidental rather than principled.

  Existing calls compile unchanged: `TcpStream` satisfies the bound and is inferred from the argument. Only a caller who named one of these as a function item or turbofished it is affected.

- `WebSocketLimits`' documentation no longer claims repe "exposes no transport types in its public API". It never quite did (`accept` returns a `WebSocketStream`), and `repe::tokio_tungstenite` makes it plainly false. The honest version: the repe-owned type exists to keep the surface to knobs that carry protocol meaning and to keep out buffer fields the transport panics on, and a tokio-tungstenite major bump is a repe major bump either way.

- **`PeerRegistry::broadcast_notify_*` allocates once per peer instead of twice.** A broadcast has to copy its body once per peer regardless — each sink takes an owned `NotifyBody` — but that copy landed in a buffer sized exactly to the body. `Message::into_wire_bytes` prepends the header and query in place only when the body buffer has capacity for them, so every peer then missed the fast path and allocated a second buffer to copy the body across again. Each per-peer body is now built with room for the prefix, which halves the allocation count of a fan-out and drops the raw variant's redundant leading `to_vec`.

  Bytes moved is unchanged: both paths make two passes over the body, the second now a memmove inside one allocation rather than a copy between two. The saving is allocator pressure at fan-out, which is what a broadcast at data rate is actually spending. No API or wire change; over-reserving is harmless for an embedder sink that frames a different query.

- **The outbound frame guard no longer allocates a `String` per frame.** `frame_outbound` — the one place every response and pushed notify funnels through — copied the message's query out to a `String` before consuming the message, so it would still have the method name if the frame turned out to be over the peer's assumed limit. That name is read only to fill the `OutboundTooLarge` error hook, which almost never fires, so every frame the server sent paid for a heap allocation it immediately dropped.

  The limit is now checked before the frame is built, from the closed-form length (`HEADER_SIZE + query + body`, exactly what `into_wire_bytes` emits) rather than by measuring the built frame. That leaves the message intact through the check, so the method is read from it only on the rejection path. A refused frame is also no longer built at all — previously an over-limit message was serialized in full and then thrown away, which is the case where that buffer is at its largest.

  No API, wire, or behavior change: the guard trips at the same threshold, and reports the same method, size, and limit. A new unit test pins the threshold end to end so the closed form and the framer cannot drift apart.

- **The lockfile resolves `beve` to 7.3.0** (from 7.1.0). The requirement in `Cargo.toml` stays `beve = "7"`, which already admitted it, so this is lockfile-only and downstreams resolve their own — but unlike the last bump, this one is not inert. 7.2.0 and 7.3.0 carry fixes and a behavior change that reach code repe calls.

  Every bulk primitive repe names directly is untouched. `to_writer_complex_slice`, `complex_slice_size`, `read_complex_slice`, `read_typed_slice`, and the aligned typed-slice family all live in beve's `fast` module, whose only diff across these two releases is a doc comment. `MessageBuilder::body_complex_slice`, `write_message_complex_slice`, and the SVS bulk modes emit and accept the same bytes they did on 7.1.0. (7.2.0's headline "complex arrays in one bulk copy" speedup is about beve's `serde(with)` helpers; `to_writer_complex_slice` was already a single `write_all`.)

  What does reach repe is the generic serde path — `to_writer_streaming` / `from_reader_streaming` behind SVS value mode, and `from_slice` / `to_vec` behind every JSON-or-BEVE body — where the payload type is the caller's:

  - **An untrusted element count can no longer abort the process** (beve 7.3.0). The `beve::typed::*` and `beve::complex_array::*` visitors sized their result `Vec` from a sequence `size_hint` taken straight off the wire, so a 27-byte body claiming 2^55 elements reached `Vec::with_capacity`, which aborts rather than returning an error. A repe server decodes bodies it did not write, so a handler whose payload struct carries either annotation was one malformed frame away from an abort no `Result` could catch. The reserve is now capped at 8 MiB and grows with what actually arrives.

  - **`Complex<f16>` and `Complex<bf16>` survive SVS value mode** (beve 7.2.0). `to_writer_streaming` rejected every `Complex<bf16>` as `Mismatch("invalid complex payload size")`, and `from_reader_streaming` refused both half widths as `Unsupported("unsupported complex float width")` — so a value-mode stream of a type holding either field failed at the producer or the consumer, on payloads the buffered codec handled. Both now decode the four float widths the slice deserializer always has.

  - **A complex array whose component class differs from the field's now decodes** through serde instead of returning `Error::Mismatch` (beve 7.3.0) — a complex `i16` payload into a `Vec<Complex<f32>>` field converts in one pass. This is a loosening, so anything that leaned on the old strictness as a schema check needs its own check. `Message::complex_slice` is deliberately *not* part of this: it calls `read_complex_slice`, where naming `T` is the whole interface and a class mismatch is still an error. The two disagreeing is intended, not an oversight.

  The `mat` feature moved to hdf5-pure 0.39 over the same span, which repe does not enable.

  The `wasm-tests` lockfiles move in step, and pick up the stale `repe` path-dep version they still recorded as 7.0.2.

## [7.1.0] - 2026-08-10

### Added
- **`WasmClient::subscribe_notifies()` / `unsubscribe_notifies()`.** The browser client matched every inbound frame by request id and dropped anything unmatched, so a server-pushed notify could not reach the application at all — push-driven protocols were native-only. Notifies are now routed to a subscriber, checked before the correlation map so they cannot collide with an in-flight request sharing the same id.

  Same one-subscriber contract as `WebSocketClient`, including `Err(AlreadySubscribed)` rather than a silent steal. The receiver is a `futures_channel::mpsc::UnboundedReceiver<Message>` — a `Stream`, drained with `StreamExt::next()` — because tokio does not build for `wasm32-unknown-unknown`. See [docs/websocket.md](docs/websocket.md#from-the-browser).

### Fixed
- **A notify subscription now ends when the connection does,** on both `WebSocketClient` and `WasmClient`. The channel was left open when the socket closed or errored, so a consumer driven only by server pushes — which never issues a request and so never sees a transport error — waited forever on a message that could not arrive. The stream now terminates (`recv()`/`next()` yields `None`), which is that consumer's signal to reconnect. A malformed inbound frame does not end it: the socket is still live and the next frame may be a valid notify.

### Changed
- `AlreadySubscribed` moved from `websocket_client` to `error`, so both notify-capable clients name one type. `repe::AlreadySubscribed` and `repe::websocket_client::AlreadySubscribed` both still resolve, and it is now exported whenever either `websocket` or `websocket-wasm` is enabled. Its `Display` text drops the `WebSocketClient` mention: "a notify subscription is already active on this client".

- CI runs the browser client in a real browser. 7.0.2 added a job proving `wasm_client` compiles; nothing ran it, so its failure mode became "builds and silently does nothing" — which is how notifies were unreachable in the first place. A headless-Chrome job now drives it against a scripted server. Test-only, no effect on the published crate; see [wasm-tests/README.md](wasm-tests/README.md).

## [7.0.2] - 2026-08-10

### Fixed
- **The `websocket-wasm` feature compiles again.** It has not built for `wasm32-unknown-unknown` since 6.0.0, failing with `error[E0004]: non-exhaustive patterns: &RepeError::MessageTooLarge { .. } not covered` — 6.0.0 added that variant and `clone_fatal_error_for_waiter` in `wasm_client.rs` was never extended to carry it to a waiter. A browser client could not depend on this crate's own WebSocket transport at all across 6.0.0, 6.1.0, 7.0.0 and 7.0.1; the workaround was to frame by hand over the public `Message` API. The variant is now cloned through like every other.

  No `_` arm was added. `RepeError` is this crate's own, so `#[non_exhaustive]` does not force a wildcard on an in-crate match, and the compile error a new variant produces here is worth keeping — it is the reminder to decide how that variant reaches a waiter. What was missing is a build that surfaces it.

- A `collapsible_if` clippy warning in `wasm_client.rs`, unreported for the same reason.

### Changed
- CI builds `wasm32-unknown-unknown`. `wasm_client` is gated on `all(feature = "websocket-wasm", target_arch = "wasm32")`, so the existing `--all-features` job on ubuntu leaves that cfg false and never compiles the module — which is how it stayed broken across four releases. The new job runs clippy at `-D warnings` for `--features websocket-wasm` and for the core crate with no features.

  It deliberately does not run `--all-features`: `websocket` pulls tokio-tungstenite and `value-stream` pulls zstd, neither of which targets wasm32. `websocket-wasm` is the feature that claims to, so it is the one held to it.

## [7.0.1] - 2026-08-10

### Changed
- The lockfile resolves `beve` to **7.1.0** (from 7.0.1). The requirement in `Cargo.toml` stays `beve = "7"`, which already admitted it, so this is lockfile-only and downstreams resolve their own.

  Nothing in it reaches repe. beve's `src/` is byte-identical between 7.0.1 and 7.1.0 — the release is a manifest change moving the optional `mat` (MATLAB v7.3 / HDF5) feature to `hdf5-pure` 0.35, plus README corrections. repe does not enable `mat`, so no repe source, wire output, or MSRV changes, and the full suite passes untouched. 7.0.1 itself was a docs.rs configuration fix.

- The interop suite's pinned Glaze tag moves to **v8.0.0** (from v7.7.1), in `interop/cpp/CMakeLists.txt`, the `interop` CI workflow, and the fixture generator's default. Dev-only: `interop/` is not in the published crate, and no repe source changes.

  Regenerating against Glaze 8 leaves **all 11 fixture frames byte-identical** — only `manifest.json`'s `glaze_version` field changes — so the REPE wire output is unaffected by the Glaze major, and `tests/interop.rs` passes untouched. Glaze's baseline compiler requirement is unchanged (GCC 13+, Clang 18+), so the CI job's `g++-14` still builds the generator.

  The reason to move: Glaze 8.0.0 is where Glaze adopted BEVE **Version 2**, the same move `beve` made in 5.0 and repe picked up in 7.0.0. Pinning against 7.7.1 meant the two ends of the suite sat on opposite sides of that line. They now match. `docs/interop.md` is updated to say so — variants are still unpinned coverage rather than a known incompatibility, and the note about matching the variant *shape* (serde's external tagging vs. a bare `std::variant` vs. `tag`/`ids`) still stands.

## [7.0.0] - 2026-08-10

### Changed
- **BREAKING:** upgraded the `beve` dependency from `3` to `7`. As in the 4.0.0 bump, beve types appear in repe's public API (`RepeError::Beve(beve::Error)`, the re-exported `beve::{BeveTypedSlice, Complex}`, and the `T: beve::BeveTypedSlice` bounds on the typed/complex body, route, and stream-pull surfaces), so a beve major is a repe major. Downstreams that also depend on `beve` directly must move to `beve 7`. Every beve primitive repe builds on — the aligned typed-array family, the streaming typed/complex writers and their size functions, the bulk `read_typed_slice` / `read_complex_slice` decoders, `to_writer_streaming` / `from_reader_streaming` / `serialized_size` — is unchanged, so there is no repe API change of its own and no MSRV move (beve 7 requires 1.89; repe's 1.96 floor comes from `uniudp`).

  Three of the four majors do not reach repe: beve 4, 6, and 7 move the optional `mat` (MATLAB v7.3 / HDF5) dependency forward, and repe does not enable that feature.

- **BREAKING (wire):** beve 5.0 is BEVE **Version 2** compliance, which changes the bytes a serde **enum** encodes to inside a BEVE body. Nothing repe itself puts on the wire is affected — the REPE header is hand-packed, and every body type repe defines (the SVS `open`/`next`/`cancel` messages) is a plain struct — but an application enum in a `body_beve` / `with_typed` / `call_typed_beve` body now encodes differently:

  | Variant kind | via beve 3 | via beve 7 |
  | --- | --- | --- |
  | unit | bare `u32` index (`Circle` → `0`) | the name as a string (`"Circle"`) |
  | newtype / tuple / struct | type-tag extension (`0x0E`) + index + payload | single-key object (`{"Rect": {...}}`) |

  This is what `serde_json` writes, so a repe endpoint's BEVE and JSON bodies now describe an enum the same way. Both old forms still **decode**, so a peer on this version reads bodies written by a peer on repe 6.x or earlier; the reverse does not hold. Two peers exchanging enums must therefore be upgraded reader-first, or together.

  It also matters for **C++ interop**. Glaze moved to BEVE Version 2 in 8.0.0, so this bump brings the two implementations back onto the same encoding; a Glaze peer older than that (including the v7.7.1 tag `interop/fixtures/` is generated from) reads the Version 1 form only. Nothing in `interop/fixtures/` exercises variants, so the interop suite is unaffected and still passes unchanged. See [`docs/interop.md`](docs/interop.md#a-note-on-beve-variants), which also covers the separate matter of the variant *shape* — Version 2 fixes the encoding, not whether a Rust enum and a `std::variant` were configured to describe themselves the same way.

- beve 5.0.1/5.0.2 additionally tightened the serializer: a hand-written `Serialize` whose body does not match the field or entry count it declared to `serialize_struct` / `serialize_map` is now an error at `end` rather than an object header promising data the reader never finds. This affects an application body type with a hand-written impl; a `#[derive(Serialize)]` type (including one using `skip_serializing_if`) cannot trip it.

## [6.1.0] - 2026-07-27

### Changed
- **MSRV raised to 1.96** (from 1.89), required by `uniudp` 1.1.0 and later. Cargo's `rust-version` has no per-feature granularity, so the crate declares the highest floor any feature needs; in practice the new floor only binds when the optional `fleet-udp` feature is enabled, and the rest of repe still compiles on 1.89.
- Depend on `uniudp = "1.2.1"` (raised from `1.0.0`) behind `fleet-udp`. 1.1.0 refreshed its crypto/RNG stack (`hmac` 0.13, `sha2` 0.11, `rand` 0.10, `mio` 1.2); 1.2.x replaced the unmaintained `reed-solomon-erasure` with `reed-solomon-engine` 0.2 and added receiver accessors for the pending reassembly backlog. The UDP wire format is unchanged, so a peer on the older version still interoperates. No repe API change: repe drives only uniudp's sender.
- The yanked `spin 0.9.8` is gone from the lockfile, which silences the yank warning `cargo publish` emitted during the 6.0.0 release. It arrived transitively through `reed-solomon-erasure`, which the uniudp bump drops; the swap to `reed-solomon-engine` is a net removal of 13 packages from the lockfile. Lockfile-only; downstreams resolve their own.

## [6.0.0] - 2026-07-20

### Added
- `WebSocketLimits` — per-connection size limits for the WebSocket transport, with `WebSocketClient::connect_with_limits`, `WebSocketServer::with_limits`, and the associated-function variants `WebSocketServer::accept_with_limits` / `accept_with_handshake_and_limits`. Previously every REPE WebSocket connection was stuck with the underlying transport's defaults (16 MiB per frame, 64 MiB per message), because repe passed no configuration to the handshake and exposed no way to supply one.

  The type is repe-owned rather than a re-export of the transport's own config struct: repe exposes no transport types in its public API, and keeping it that way means a transport version bump stays an internal detail instead of a repe breaking change. It also confines the surface to the knobs that carry protocol meaning, leaving out buffer-tuning fields that make the transport panic when set inconsistently.

  Note the direction. WebSocket size limits are enforced by the **reader**, so `max_incoming_frame_size` / `max_incoming_message_size` govern what this endpoint accepts, not what the peer will. A larger payload in either direction requires the *receiving* end to be configured for it.

- An outbound guard, so an undeliverable message reports instead of vanishing. Because limits are read-side and no handshake field carries them, a sender cannot discover what the peer accepts: an oversized message left the sender cleanly, the peer's reader rejected it and closed the connection, and the sender observed a dropped socket with no error at all. A reconnecting client that re-requested the same payload looped forever.

  `WebSocketLimits::assumed_peer_frame_limit` is checked before sending. A refused **response** is replaced with an `ErrorCode::InternalError` response carrying the same request id, so the caller gets a real REPE error and the connection stays up; a refused **notify** is dropped, having no response to carry the failure. Both reach the server's `on_error` hooks as the new `ConnectionError::OutboundTooLarge`. On the client, an oversized request fails locally with `RepeError::MessageTooLarge` before reaching the wire.

  It defaults **on**, at the transport's own 16 MiB default frame size. This cannot break a working deployment: before `WebSocketLimits` existed there was no way to raise a peer's inbound limit, so no message above it has ever been deliverable to a default peer, and the guard converts a failure that was already happening silently into one that says so. The case to know about is a peer that is not this library, configured with a larger inbound limit; raise `assumed_peer_frame_limit` to match it, or clear it to disable the check.

  Limits set here reach only connections repe upgrades itself. A connection the embedder upgraded and handed to `SharedWebSocketServer::serve_connection` carries whatever inbound limits that upgrade was given and cannot be retrofitted, since the transport fixes them at construction and exposes no setter. The outbound guard, being repe's own, applies on every path.

- `SharedWebSocketServer::accept` / `accept_with_handshake` / `limits`. The associated `WebSocketServer::accept` takes no `self`, so in an embedder-owned accept loop it cannot see a builder-set `with_limits` and upgrades with the defaults instead: the limits appear configured and silently are not. These `&self` methods close that gap; prefer them for one-port co-hosting.
- `proxy_connection_with_limits`, so a proxy's forwarding writer can refuse a response the downstream reader would reject.

### Changed
- **BREAKING:** `RepeError` is now `#[non_exhaustive]` and gains a `MessageTooLarge { size, limit }` variant. Downstream matches need a `_` arm. This is the one-time cost of making every future error variant a non-breaking addition.
- **BREAKING:** `ConnectionError` gains `OutboundTooLarge { method, size, limit }`. It is already `#[non_exhaustive]`, so a match with a `_` arm is unaffected.
- **BREAKING:** `proxy_connection` now applies the outbound guard at the default 16 MiB, forwarding an error response rather than a message the downstream reader would reject. Use `proxy_connection_with_limits` to change or disable it.

## [5.0.0] - 2026-07-15

### Added
- `RouterValueStreamExt::with_writer_stream::<W, F>(format, resolve, opts)` — the write-side SVS producer, the counterpart to `with_reader_stream`'s opaque `Read`. `resolve` hands back a closure `W: FnOnce(&mut dyn Write)` that *owns* the stream sink, so the app serializes/emits the body in a **single pass** rather than materializing it first. Its headline over `with_value_stream` (where the engine owns the encode and the app never sees the sink) is a **single-pass digest seam**: the closure can tee its bytes through a hasher while it encodes and append a trailing digest, so end-to-end content integrity falls out of the one streaming pass with no double-encode. Such a stream is app-framed `payload || digest` tagged `BodyFormat::RawBinary` (pull with `pull_to_file_trailer_verified`, below), not a standard BEVE/JSON tag with a trailer; a plain trailer-free write can instead be tagged `BodyFormat::Beve` and pulled with `pull_value`. The digest covers the **logical** (pre-compression) bytes; `opts.compression` is transparent end to end. Demonstrated in `examples/writer_stream_digest.rs`; tested in `tests/value_stream.rs`.
- Consumer ergonomics for the in-stream digest hatch and the format-agnostic pull:
  - `pull_to_file_trailer_verified` / `pull_to_file_trailer_verified_async` — pull an app-framed `payload || digest` stream, hold the last `trailer_len` bytes back as the claimed trailer, hash only the payload prefix into a caller `digest`, and after a clean EOF run `verify(digest, &trailer)`; an `Ok` commits the **payload alone** (trailer stripped) atomically, an `Err` commits nothing. Removes the plain-pull + split + verify + re-truncate boilerplate the `with_writer_stream` seam otherwise forced on the consumer; the in-stream-digest counterpart to `pull_to_file_verified_async` (which hashes the whole stream against an out-of-band digest and keeps every byte).
  - `pull_consume` — the synchronous, format-agnostic reader hatch (hands the decompressed logical content to a caller closure as a blocking `Read`), the sync twin of `pull_consume_async`.
  - `pull_to_vec` / `pull_to_vec_async` — buffer the whole logical content into a `Vec<u8>` (small payloads only; large transfers should pull to a file or decode into a value).

### Changed
- **BREAKING:** the fixed header's `reserved` field is now ignored on decode per the REPE spec ("Must be zero, receivers must ignore this field") rather than rejected when non-zero, so a future REPE revision can assign meaning to those bits without breaking this receiver. The now-unproducible `RepeError::ReservedNonZero` variant is removed.

## [4.0.0] - 2026-07-13

### Changed
- **BREAKING:** upgraded the `beve` dependency from `2.5` to `3`. beve's core ser/de API is unchanged from 2.x, so this introduces no repe API change of its own; the major bump is required because beve types appear in repe's public API (e.g. `RepeError::Beve(beve::Error)` and the `T: beve::BeveTypedSlice` bounds on the typed/complex stream and bulk-body routes), so a beve major is a repe major. Downstreams that also depend on `beve` directly must move to `beve 3`.

## [3.10.0] - 2026-06-18

### Added
- Producer `RouterValueStreamExt::with_reader_stream::<R: Read + Send>` — stream **opaque, already-serialized bytes** verbatim from any `Read` without re-serializing a value. Tags the stream `format = RawBinary`; with `Compression::None` the chunk stream is the source bytes verbatim, so a consumer can write a byte-identical copy. Use `R = Box<dyn Read + Send>` to serve several concrete sources from one registration. (The `open`-response `format` tag is now set per registration rather than hardcoded to BEVE.)
- Async consumers: `pull_to_file_async` (async atomic file pull — temp sibling, fsync, rename only after the terminating chunk), `pull_consume_async` (format-agnostic async escape hatch that hands the logical content to a caller closure as a blocking `Read`; a truncated transfer surfaces before the closure's value is returned), and `pull_to_file_verified_async` (tees content bytes into a caller-supplied `digest` and runs a `verify` closure after a clean EOF and before the rename, so an integrity check recomputed over the streamed bytes — compared against an out-of-band digest — can veto publication). SVS commits no digest on the wire; this is the integrity seam for callers that need one.
- Sync `StreamOutput::RawFile` and `pull_to_file`, for symmetry with the async file pull.

## [3.9.0] - 2026-06-11

### Added
- A **Serialized Value Stream (SVS)** download path behind the optional `value-stream` feature: stream a large (multi-GB even compressed) BEVE-serialized, optionally zstd-compressed in-memory value over an ordinary REPE request/response connection. Flow control is the round trip itself (no ACK window, no unbounded queue), and the producer never materializes a full serialized or compressed copy. Implements the cross-language [SVS spec](https://github.com/repe-org/REPE/blob/main/serialized-stream-protocol.md).
  - **Producer** (via `RouterValueStreamExt` on `Router`): `with_value_stream::<T: Serialize>` (arbitrary serde value), `with_typed_value_stream::<T: BeveTypedSlice>` (a `Vec<T>` bulk typed array), and `with_complex_value_stream::<T>` (a `Vec<Complex<T>>`, e.g. IQ buffers). Each registers `/_svs/{open,next,cancel}` backed by a bounded per-stream session (serialize → optional zstd → chunk into a bounded channel). The `next` handler reports `Execution::OffReader` and blocks for backpressure on the sync `Server` and the WebSocket server; it is not for the inline async server (documented in the module).
  - **Sync consumer**: `pull_value::<T>` / `pull_to_beve_file` / `pull_to_beve_zst_file` / general `pull_stream` (in-memory value, decompressed `.beve` file, raw `.beve.zst` file — file modes commit atomically via temp-then-rename only after the terminating chunk), plus the bulk `pull_typed_slice::<T>` / `pull_complex_slice::<T>` (memcpy decode straight into the result `Vec`).
  - **Async consumer**: `pull_value_async` / `pull_typed_slice_async` / `pull_complex_slice_async`, generic over a sealed `AsyncSvsClient` transport — they drive either the async-TCP `AsyncClient` or (with the `websocket` feature) the `WebSocketClient`. The blocking `Read`-based decoders run on a `spawn_blocking` task draining a bounded channel fed by an async `next`-issuing task, so the runtime is never parked and the channel bound is the backpressure.
- `Router::with_erased_handler` — the low-level escape hatch (register a raw `HandlerErased` returning a fully custom `Message`) the SVS handlers are built on; broadly useful beyond SVS.

### Changed
- Depend on `beve = "2.5"` (raised from `"2.3"`), the floor for the `read_typed_slice_from_reader` / `read_complex_slice_from_reader` streaming bulk decoders backing the bulk pull receivers.

## [3.8.0] - 2026-06-09

### Added
- `MessageBuilder::body_aligned_typed_slice(&[T])`, the zero-copy counterpart of `body_typed_slice`: it encodes a contiguous numeric slice as BEVE's *aligned* typed array, padded (via `beve::write_aligned_typed_slice_at`) so the payload lands on an `align_of::<T>()` boundary at its eventual `HEADER_SIZE + query.len()` frame offset. This makes the body borrowable as `&[T]` by a `Router::with_typed_slice_ref` server. The query must be set before this is called (so the offset is known); if it is set afterward the padding is for the wrong offset and the server falls back to a bulk copy (still correct, just not zero-copy). The body carries the same `into_wire_bytes` wire-prefix headroom as `body_typed_slice`, and the alignment survives that shift.

### Changed
- `AsyncClient::call_typed_slice_aligned` / `Client::call_typed_slice_aligned` now frame their request through `MessageBuilder::body_aligned_typed_slice` and the ordinary `call_with_body_and_timeout` path, replacing the dedicated in-place frame builder and raw-frame dispatch helpers introduced in 3.7.0. The wire bytes are byte-for-byte identical, so this is an internal simplification with no observable behavior change; it removes the parallel send path now that `beve = "2.3"` can pad a standalone body for an arbitrary frame offset.
- Depend on `beve = "2.3"` (raised from `"2.2"`), the floor for `write_aligned_typed_slice_at` (the offset-aware aligned writer).

## [3.7.0] - 2026-06-09

### Added
- A zero-copy borrowing route for bulk numeric requests, realizing the aligned-typed-array path noted as future work in 3.6.0 (now that `beve = "2.2"` implements it). `Router::with_typed_slice_ref::<T, R, _>(path, |&[T]| -> Result<Vec<R>, _>)` hands its handler a `&[T]` borrowed straight out of the connection's receive buffer, with no allocation and no element copy on decode, when the request arrives in BEVE's *aligned* typed-array wire form and the buffer base is aligned to `align_of::<T>()` (the common case for the reused per-connection buffer on little-endian targets). The client opts into that wire form with `AsyncClient::call_typed_slice_aligned` / `Client::call_typed_slice_aligned` (with `_with_timeout` variants), which frame the request in place so the padding run places the payload on a `T` boundary within the frame. The aligned body is a distinct BEVE type from the regular typed array, so it pairs specifically with a `with_typed_slice_ref` route; a plain `with_typed_slice` / serde route rejects it with `ErrorCode::InvalidBody` rather than misreading. Conversely a `with_typed_slice_ref` route is a drop-in *superset* of `with_typed_slice`: it transparently accepts the regular (unaligned) typed array sent by `call_typed_slice` and the serde (`call_typed_beve`) path, bulk-copying those into the borrowed `&[T]`, and it falls back to a bulk copy for an aligned body whenever the buffer is not aligned, so correctness never depends on the alignment landing. The response is framed exactly as `with_typed_slice` frames it (a regular typed array), so it interoperates with every client on the way back out. A wrong element class/width surfaces as `RepeError::Beve` rather than being misread. See `tests/typed_slice_zero_copy.rs`.

### Changed
- Depend on `beve = "2.2"` (raised from `"2.1"`), which adds the aligned typed-array primitives (`write_aligned_typed_slice` / `aligned_typed_slice_size` / `read_aligned_typed_slice` / `read_aligned_typed_slice_ref`) backing the new borrowing route.

## [3.6.0] - 2026-06-09

### Added
- A bulk numeric route that carries the `body_typed_slice` / `decode_typed_slice` fast path end-to-end through the high-level client and server, so large contiguous `f32`/`f64` (and other scalar) arrays no longer pay serde's per-element walk on the request and response. `Router::with_typed_slice::<T, R, _>(path, |Vec<T>| -> Result<Vec<R>, _>)` decodes the request body with one bounds-checked bulk copy (`beve::read_typed_slice`) and frames the `Vec<R>` response with one bulk write (`MessageBuilder::body_typed_slice`), implementing both the owned and the allocation-free borrowing (`handle_view`) dispatch paths. `AsyncClient::call_typed_slice` / `Client::call_typed_slice` (with `_with_timeout` variants) are the client counterpart: they send a `&[T]` body via the bulk writer and decode the `Vec<R>` response via the bulk reader. The wire bytes are byte-for-byte identical to the serde (`with_typed` / `call_typed_beve`) path, so a `with_typed_slice` route and a serde client (or vice versa) interoperate freely; only the encode side is changed, and only where the element type is statically a `BeveTypedSlice` scalar. A request whose body is not a BEVE typed array of `T` is rejected with `ErrorCode::InvalidBody`; a typed array of the wrong element class/width surfaces as `RepeError::Beve` rather than being misread. Measured end-to-end against gRPC+protobuf over loopback, this turns the large-`f64` case from a ~2x loss into a 6-8x win (e.g. 1M `f64` echo: 47.6 ms → 5.0 ms), and widens the integer-array win further (1M `i64`: 40.9 ms → 5.2 ms). See `tests/typed_slice_fastpath.rs`. This path decodes into an owned `Vec<T>` with one bulk `copy_nonoverlapping`; it is not zero-copy. `body_typed_slice` emits BEVE's regular (unaligned) typed array, whose payload follows a 1-byte tag and a varint length and so is not `T`-aligned, so a borrow would be unsound and the copy is the floor here. BEVE's separate *aligned* typed-array type (which pads the payload to `alignof(T)` specifically for zero-copy) is the path to a true borrowing decoder, but it is not yet implemented in the `beve` Rust crate and would use a distinct, non-serde-identical wire format; that remains possible future work.

### Changed
- The async server (`AsyncServer::serve`) now sets `TCP_NODELAY` on every accepted connection, matching what `AsyncClient::connect` already does on its side and what the synchronous `Server` already does by default (its configurable `tcp_nodelay`, default on). A buffered response is flushed as a single write, so coalescing it with delayed-ACK only added latency, most visibly on large multi-segment numeric bodies. Like `AsyncClient`, the async server enables it unconditionally with no opt-out (the synchronous `Server`'s `tcp_nodelay` toggle is a separate lineage); there is no known reason to want Nagle on this path. A `set_nodelay` failure is non-fatal and left unpropagated (the socket still works, just with Nagle on).

## [3.5.2] - 2026-06-09

### Added
- Cross-language wire-compatibility test suite against the canonical C++ REPE implementation (Glaze). A committed C++ generator (`interop/cpp/`) links Glaze and emits authentic REPE v1 frames; those bytes and a manifest live under `interop/fixtures/`; `tests/interop.rs` asserts parse parity, body decode (JSON / BEVE object / BEVE typed numeric / UTF-8 error), byte-identity round-trip, and — for protocol-defined layouts — from-scratch encoder parity. Notably, repe-rs's `body_typed_slice` BEVE numeric arrays and its error frames are byte-identical to Glaze's. A gated `interop` CI workflow rebuilds the generator from the pinned Glaze tag, regenerates, and fails on drift. No library code or public API changed; `interop/` is excluded from the published crate. See `docs/interop.md`. Both implementations are REPE v1 (48-byte header), the shipping spec; a v2 (32-byte header) exists only as an unreleased work-in-progress branch of the spec, so adopting it is separate future work.

## [3.5.1] - 2026-06-09

### Changed
- `MessageBuilder::body_typed_slice` / `body_complex_slice` now allocate their body buffer with `HEADER_SIZE + query.len()` of spare capacity reserved after the encoded payload, so shipping the built message through `Message::into_wire_bytes` reuses that allocation in place instead of allocating a fresh frame buffer. The encode itself stays a single allocation (the slice is written into the pre-sized buffer via `beve::to_writer_*_slice` rather than `to_vec_*_slice`), and the wire bytes are unchanged. The headroom is effective whenever the body is encoded with the final query already in place — the common `.query_*(..).body_typed_slice(..)` order, and also the query-less case (the reserved prefix is then just the header). Only setting a non-empty query *after* the body misses it, falling back to a fresh frame exactly as before. Only affects the build-a-Message-then-`into_wire_bytes` outbound pattern; the streaming `write_message_typed_slice` / `write_message_complex_slice` path was already zero-buffer.

## [3.5.0] - 2026-06-09

### Added
- `write_message_complex_slice`, the complex counterpart of `write_message_typed_slice`: it frames a REPE message whose entire body is a complex numeric array (`&[Complex<T>]`) straight to a sink with no intermediate body buffer, sizing it in closed form (`beve::complex_slice_size`, O(1)) and writing the interleaved `(re, im)` payload in one bulk write. The wire bytes are identical to a `MessageBuilder::body_complex_slice` message framed with `write_message`; decode with `Message::decode_complex_slice`. This closes the streaming-framing gap left in 3.4.0, where complex bodies had no zero-buffer path and had to be built and allocated whole via `body_complex_slice`.

### Changed
- Depend on `beve = "2.1"` (raised from `"2"`), which adds the `to_writer_complex_slice` / `complex_slice_size` streaming primitives backing the new framing helper.

## [3.4.0] - 2026-06-08

### Added
- A whole-body fast path for high-throughput numeric payloads (typed numeric and complex arrays), bypassing serde's per-element walk. `MessageBuilder::body_typed_slice(&[T])` / `body_complex_slice(&[Complex<T>])` encode a contiguous slice as a BEVE typed/complex array in one bulk write; `Message::decode_typed_slice::<T>()` / `decode_complex_slice::<T>()` read it back in a single bounds-checked bulk copy. The bytes are byte-for-byte identical to the serde `body_beve(&Vec<T>)` path, so the bulk and serde paths interoperate; the decoders also read serde-produced bodies and vice versa. `write_message_typed_slice` frames a numeric body straight to a sink with no intermediate body buffer, sizing it in closed form (`beve::typed_slice_size`, O(1)) and writing the payload in one bulk write. New `RepeError::UnexpectedBodyFormat { expected, got }` (maps to `ErrorCode::InvalidBody`) reported by the decoders when the body is not `BodyFormat::Beve`. `beve::{BeveTypedSlice, Complex}` re-exported at the crate root. See `docs/numeric-bodies.md` and `examples/typed_numeric_body.rs`.
- `benches/wire_serialization.rs` gains `typed_numeric_framing_f64`, comparing serde streaming framing against the typed-slice fast path across body sizes (~14x at 64 elements rising to ~25-33x once memory-bandwidth bound).

### Changed
- Depend on `beve = "2"` (raised from 1.4) and drop `default-features = false`. beve 2.0's default feature set is already lean -- its heavy MATLAB/HDF5 `mat` interop is now opt-in -- so repe no longer needs to disable default features, and 2.0 carries the `read_typed_slice` / `read_complex_slice` bulk decoders backing the new numeric body path.
- `Message::json_body` and `beve_body` now report a wrong-format body as `RepeError::UnexpectedBodyFormat` (→ `ErrorCode::InvalidBody`), the same structured error the new bulk decoders use, instead of synthesizing a `serde_json` / BEVE error tagged `ParseError`. One error shape for "wrong body format" across all four body decoders. Code matching on the previous `RepeError::Json(_)` / `RepeError::Beve(_)` for that specific case should match `UnexpectedBodyFormat`; a successful decode is unaffected.

## [3.3.0] - 2026-05-30

### Added
- Borrowing, zero-allocation read path for server dispatch. `read_message_into(reader, &mut Vec<u8>)` and `read_message_into_async` read a full frame into a reusable per-connection buffer; pair with `MessageView::from_slice`/`from_slice_exact` (the latter rejects trailing bytes, for one-message-per-frame transports) to dispatch via the new `HandlerErased::handle_view(&MessageView, &CallContext)` without the per-request query and body `Vec` allocations an owned `Message` requires. `handle_view`'s default materializes an owned `Message` (`MessageView::to_message`) and delegates to `handle_with_ctx`, so every existing handler works unchanged; the built-in JSON and typed handlers (`with_json` / `with_typed`) override it to decode straight from the borrowed body.
- A measurement harness for the server hot path: `tests/allocations.rs` pins the per-request allocation count of read → dispatch → echo → write with a thread-local counting `#[global_allocator]` and `assert_eq` budgets (so a regression up *or* a deliberate reduction down is caught), and `benches/server_lifecycle.rs` measures the same cycle end-to-end.

### Changed
- The TCP, async, and WebSocket-inline servers now dispatch off the borrowing read path. The TCP and async servers keep one reusable read buffer per connection and frame the response echoing the query straight out of it, so steady-state request framing for `with_json`/`with_typed` routes allocates nothing (per-request allocations 5 → 3; `server_lifecycle` `json_echo` ~341→316 ns, `typed_sum` ~176→137 ns). The WebSocket inline path borrows the request from the tungstenite payload, eliminating the request body copy (5 → 4; the response query is still copied because the outbound channel carries owned messages framed later by the writer task); the WebSocket off-reader path keeps the owned decode (its spawned task outlives the read buffer).
- Per-response query echo is now a buffer move (owned/WebSocket path) or a borrow (TCP/async), not a clone. Built-in handler responses leave the query empty and the dispatch layer supplies it; a handler that sets its own response query is left untouched. Registry JSON-pointer resolution borrows escape-free tokens (`Cow`) and builds error-path strings lazily, taking a depth-N pointer walk from ~`2N` `String` allocations to zero (`registry_value` read −33%, write −22%). Owned JSON/typed decoders parse a `Utf8`-framed body with strict `from_slice` (matching the borrowing path; drops a per-request `String`).

### Notes
- The 5→3 / "framing allocates nothing" win applies to `with_json`/`with_typed` routes, which override `handle_view`. Context-aware, struct, registry, and middleware-wrapped routes use the owning `handle_view` default (a correct owned copy) and keep the owned allocation count until overridden; middleware-wrapped routes are structurally bound to the owned path because the `Middleware` trait takes `&Message`.
- Behavior change to a public trait method, documented on `HandlerErased`: built-in handlers' `handle` / `handle_with_ctx` now return a response whose `query` is *empty* — the dispatch layer fills it. Code that calls a handler directly (e.g. via `Router::get`) and needs a complete, query-echoing response should build it with `create_response`, which echoes the query itself. The owned/WebSocket error path and success frames are byte-identical to before. (One edge improvement: a `Utf8`-framed body containing invalid UTF-8 is now rejected strictly rather than lossily substituted.)

## [3.2.0] - 2026-05-29

### Added
- One-port co-hosting classifier: `is_websocket_upgrade(&TcpStream) -> io::Result<bool>` (re-exported at the crate root). A non-destructive WS-vs-HTTP sniff that lets a single accept loop fork REPE WebSocket upgrades to `SharedWebSocketServer::serve_connection` and everything else to the embedder's own HTTP handler on one TCP port. It uses `TcpStream::peek`, so the request bytes stay in the socket buffer for the subsequent `WebSocketServer::accept` (which replays the handshake from the start of the stream); it matches the `websocket` token only inside an `Upgrade` header's value, so `GET /websocket-status` is not misclassified, and `accept` still performs the authoritative RFC 6455 validation on the `true` branch.
- `examples/websocket_cohosting.rs`: a runnable end-to-end demo that fills in both halves of the co-hosting pattern — the classifier plus a minimal, dependency-free `serve_http` (`/healthz` plus a JSON `GET`) — driving both forks against one port. Linked from the co-hosting section of `docs/websocket.md`.
- `PeerRegistry` alias index, for an embedder whose client identity is its own key (a UUID, a token) rather than the server-minted `PeerId`: `alias(peer_id, key)`, `get_by(&key)` (generic borrowed lookup like `HashMap::get`), `key_for(peer_id)`, and `aliases_for(peer_id)`. `PeerId` stays canonical, so `with_peer_registry` auto-insert is unchanged. A reverse index lets `remove` purge a peer's aliases without scanning the forward map, so disconnect cleanup stays automatic; `key_for` / `aliases_for` map a `PeerId` from a `broadcast_notify_*` result map back to the embedder's identity. Replaces hand-maintaining a parallel `key -> PeerId` map.
- `HandshakeContext` and `WebSocketServer::on_peer_connect_with_handshake(f)` (both re-exported at the crate root): a connect hook that also receives the upgrade request's `path()`, `query()`, and `header(name)`, so an embedder whose key rides in the handshake can derive it and `alias` the freshly-inserted peer at connect time. It fires *after* the plain `on_peer_connect` hooks, so a `with_peer_registry` insert has already landed and the peer is present for `alias`. The context is threaded through the built-in `serve*` loops and the new co-hosting methods `WebSocketServer::accept_with_handshake`, `SharedWebSocketServer::serve_connection_with_handshake`, and `serve_connection_with_cancel_and_handshake`. Capture is pay-for-what-you-use: the accept path builds a `HandshakeContext` only when such a hook is registered (or `accept_with_handshake` is called).

### Notes
- Strictly additive. No existing signature changes; `PeerRegistry::new()` and the existing `accept` / `serve_connection` paths are untouched.
- `HandshakeContext` is built for keying off an auth/token header, so two properties guard that use: `header(name)` is fail-closed — a value that is not valid (visible ASCII) text is dropped at capture rather than lossily replaced with U+FFFD, so a non-ASCII token reads as absent instead of a silently corrupted key — and its `Debug` is hand-written to redact header values and the query string, so a routine `tracing::debug!(?ctx)` cannot leak the credential the use case keys on.
- `is_websocket_upgrade` peeks only the connection's first readable chunk (bounded to 1 KiB). A false positive is caught by `accept`'s authoritative validation; a false negative (an `Upgrade` header split across TCP segments or beyond the peek window) routes to the HTTP handler with no recovery. The peek also blocks until the client sends a byte, so a production embedder should wrap it in a `tokio::time::timeout` to keep a silent client from pinning the task.
- The larger surfaces remain intentionally deferred per the crate's "hold higher-level surfaces until a second distinct consumer exists" discipline: a built-in HTTP route table, a generic `PeerRegistry<K>`, and an opaque `PeerHandle` tag.

## [3.1.0] - 2026-05-28

### Added
- `Message::into_wire_bytes(self) -> Vec<u8>` — consuming counterpart to `to_vec` for outbound paths where the message is shipped to a sink that takes an owned `Vec<u8>` (the built-in WebSocket writer and `UdpClient` send paths). Fast path is zero new allocations and one in-place body memcpy via `copy_within` when the body already has capacity for the `HEADER_SIZE + query.len()` prefix; opt in by constructing bodies via `Vec::with_capacity(body_len + HEADER_SIZE + query.len())` and feeding them to `MessageBuilder::body_bytes`. Slow path falls back to a fresh allocation matching `to_vec`'s cost while still releasing the original query and body buffers as soon as the wire bytes are produced. `Message::to_vec` is unchanged.
- Two `criterion` benches under `benches/`: `wire_serialization` measures `to_vec` against both `into_wire_bytes` paths across body sizes from 64 B to 4 MiB; `route_dispatch` measures `Router::get` and full `get + handle` cycles across plain HashMap, `Registry`-backed, and `RepeStruct` routes, each with and without an attached middleware. Pulled in as a `dev-dependencies` entry only.

### Changed
- Routing dispatch is now allocation-free per request on registry and struct routes. `RegisteredRegistry` and `RegisteredStruct<T, L>` implement `HandlerErased` directly: each registration builds one handler `Arc<dyn HandlerErased>` once and `Router::get` is now a prefix check (or HashMap probe) plus a single `Arc::clone`. Previously every registry hit allocated a fresh `String` for the JSON pointer and an `Arc::new(RegistryRequestHandler { ... })`; every struct hit allocated a `Vec<String>` from `json_pointer::parse`, a `String`, and an `Arc::new(StructRequestHandler { ... })` — all gone. The escape-free struct fast path also drops segment `&str`s into a 16-slot stack buffer with heap overflow, skipping the `Vec<String>` from `json_pointer::parse` entirely for the typical 1–4 segment pointer.
- Middleware wrapping hoisted from `Router::get` to registration time. Each route entry carries both a `raw` and a `dispatched` handler `Arc`, where `dispatched` is `raw` already wrapped in any active `MiddlewarePipeline`. `Router::get` returns `dispatched` directly, so plain routes pay nothing per dispatch for middleware they do not touch and middleware-equipped routes no longer pay an `Arc::new(MiddlewarePipeline)` per request. `register_middleware` rebuilds the `dispatched` slot across every existing route so semantics stay uniform; cost is `O(N·K)` for `N` middleware registrations after `K` routes, paid only at builder time.
- WebSocket writer (`writer_task` and the proxy forwarding path) and `UdpClient::send` switch from `Message::to_vec` to `Message::into_wire_bytes`, picking up the slow-path savings without callers needing to change anything. Behavior is byte-identical; the fast path activates if callers opt in with a pre-reserved body.

### Fixed
- The `repe` CLI now compiles under `cargo check --all-features`. v3.0.0 added `#[non_exhaustive]` to `BodyFormat`, `QueryFormat`, and `ErrorCode` but did not update the bin's response-decoding match; bin targets compile as a separate crate so the attribute applies across the boundary, and `cargo check --all-features` failed on the bin until the unknown-variant wildcard landed.

### Notes
- Existing `create_response` builders produce `Vec<u8>` bodies with `capacity == len`, so today's WebSocket-server outbound responses land on the `into_wire_bytes` slow path (same memcpy cost as `to_vec`, with the body buffer released sooner). The fast path is available for callers that opt in — most relevantly `repe::stream` chunk producers building their own response bodies, where the prefix reservation removes the per-chunk body copy entirely.
- Bench numbers under `cargo bench --quick`: `router_get/plain_with_middleware` matches `router_get/plain` (~33 ns), `router_get/registry|struct` is ~10 ns, and `wire_serialization/into_wire_bytes_fast` is roughly 1.5× the throughput of `to_vec` at 4 KiB and 7× at 64 KiB.

## [3.0.0] - 2026-05-27

### Added
- Off-reader handler cancellation on `CallContext`: `CallContext::is_cancelled()` (non-blocking) and `CallContext::cancelled()` (a `Future`) report/resolve when the calling peer disconnects or the server is shutting down. A long `_blocking` handler should poll `is_cancelled()` at loop boundaries and return early to free its `spawn_blocking` thread instead of running pointless work to completion. Both degrade to a never-cancelling no-op on peer-less transports (TCP servers, in-process dispatch), mirroring `CallContext::peer()` returning `None`. Backed by a per-connection `tokio_util` `CancellationToken` cancelled from the disconnect `Drop` guard; the backing type is not exposed. Complements — does not replace — the `on_peer_disconnect` → `TransferControl::cancel` path, which remains the only thing that wakes a producer parked in `wait_for_credit`.
- Graceful connection drain: `WebSocketServer::serve_with_graceful_drain(addr, path, shutdown, drain_timeout)` and `serve_listener_with_graceful_drain(listener, path, shutdown, drain_timeout)`. On shutdown they stop accepting, cancel in-flight off-reader handlers, await already-accepted connections (tracked in a `JoinSet`) until `drain_timeout`, then abort whatever remains. Aborting a connection tears down its writer task too, so the server-level `drain_timeout` supersedes the per-connection 5 s writer drain. The turnkey counterpart to `serve_with_shutdown`, which still returns immediately and detaches connections.
- `ShutdownToken` (re-exported at the crate root) and `SharedWebSocketServer::serve_connection_with_cancel(ws, &token)`, for a one-port co-hosting embedder that owns its own accept loop: create one token, serve each connection through it, and call `token.cancel()` on shutdown to wake every connection's in-flight off-reader handlers while the embedder drains its own task set. The backing `tokio_util` token is hidden so embedders are not coupled to that crate's version.
- `WebSocketServer::on_error(f)` and the `ConnectionError` type (re-exported at the crate root): a hook for transport-level events — handshake failures, connection I/O errors (in the built-in accept loops), caught off-reader handler panics, and off-reader saturation rejections — so an embedder can route them into its own `log` / `tracing` pipeline. Composes in registration order like `on_peer_connect` / `on_peer_disconnect`. With no hook registered the server keeps its historical `eprintln!` behavior (and stays silent on saturation, which never logged). `ConnectionError::error_code()` maps the panic / saturation categories to the `ErrorCode` that reached the client.
- `ErrorCode::ResourceExhausted` (8) and `ErrorCode::InternalError` (9), in the REPE-reserved `8..4095` range between `Timeout` and `ApplicationErrorBase`. Resolves the protocol gap noted in the 2.6.0 release notes: a client can now branch retry-on-busy (saturation) vs. surface-on-error (real failure) by code alone. Additive at the library level; interoperating on these codes with other REPE implementations requires they agree on the codes' meaning.
- `examples/websocket_graceful_server.rs`: a runnable end-to-end demo of a cancellation-polling off-reader handler, an `on_error` hook, and `serve_listener_with_graceful_drain`.

### Changed
- `ErrorCode`, `QueryFormat`, and `BodyFormat` are now `#[non_exhaustive]`, so downstream `match` expressions on these protocol enums must add a wildcard (`_`) arm. This is the one-time source-breaking change behind the major version bump; in exchange, future additions to these enums — new error codes in the reserved `8..4095` range, new query/body formats — ship as non-breaking minor releases.
- A caught off-reader handler panic now responds with `ErrorCode::InternalError` and an off-reader saturation rejection with `ErrorCode::ResourceExhausted` (both were `ErrorCode::ApplicationErrorBase`). A client that matched `ApplicationErrorBase` to detect these two cases must update to the new codes.
- The three WebSocket-server `eprintln!` sites (handshake error, connection error, off-reader handler panic) route through `on_error` when a hook is registered, falling back to the same stderr output otherwise.

### Notes
- `tokio-util` (with its `rt` feature, for `CancellationToken`) is a new dependency, pulled in only with the `websocket` feature.
- The cancellation signal wakes poll-loop (`is_cancelled()`) and async (`select!` on `cancelled()`) handlers; it does **not** wake a producer parked in `TransferControl::wait_for_credit` (a `Condvar`), which is still woken only by `TransferControl::cancel` — drive that from `on_peer_disconnect` as before. The two compose: cancellation winds a handler down promptly so the drain timeout is a backstop rather than the common wait.
- A misbehaving off-reader handler that never polls its cancellation signal still holds its `spawn_blocking` thread past `drain_timeout` (a blocking thread cannot be aborted); the drain's abort tears down the connection's reader/writer, not such a handler.
- A first-class `WebSocketServer` stream-source surface (owning begin/ack/cancel/resume internally) remains deferred until a second distinct windowed-transfer consumer exists. Its prerequisites — cancellation-on-disconnect and orderly drain — now land, so it can be designed against this consumer once a second appears.

## [2.6.0] - 2026-05-26

### Added
- Off-reader handler dispatch: `Router::with_json_blocking`, `with_json_ctx_blocking`, `with_typed_blocking`, and `with_typed_ctx_blocking`. On `WebSocketServer` a `_blocking` route runs on a `tokio::task::spawn_blocking` thread so the connection's reader stays free to decode further inbound frames (ACKs, cancels, resumes) while the handler runs or parks. This is the prerequisite for driving a `repe::stream` producer directly from a request handler: the producer parks in `wait_for_credit` waiting on ACKs that arrive as inbound frames the same reader must decode, so an inline handler would deadlock at the first full window. The TCP `Server` / `AsyncServer` ignore the tag and always run inline (they have no peer and no push primitive).
- `Execution` enum (re-exported at the crate root) and `HandlerErased::execution()`, returned by the trait to tell `WebSocketServer` where to dispatch. Defaults to `Execution::Inline`, so existing handlers and custom `HandlerErased` implementors are unaffected; the `_blocking` constructors return `Execution::OffReader`.
- `WebSocketServer::with_offreader_limit(n)` and `DEFAULT_OFFREADER_LIMIT` (16): per-connection cap on concurrently running off-reader handlers. When the cap is reached a further off-reader request gets an immediate error response (the client retries) rather than blocking the reader — blocking it would stall the very ACK/cancel frames in-flight transfers need to free a slot. Pass `0` to remove the cap.
- Graceful shutdown: `WebSocketServer::serve_with_shutdown(addr, path, shutdown)` and `serve_listener_with_shutdown(listener, path, shutdown)` stop accepting once the `shutdown` future resolves and return `Ok(())`. Already-accepted connections run on their own detached tasks and are not awaited; track them via a `PeerRegistry` if you need to drain them.
- One-port co-hosting: `WebSocketServer::into_shared()` returns a cheap, cloneable `SharedWebSocketServer` (re-exported at the crate root). Pair `WebSocketServer::accept(stream, path)` (the REPE handshake) with `SharedWebSocketServer::serve_connection(ws)` to serve connections the embedder accepts itself — e.g. share one TCP port between REPE WebSocket upgrades and the embedder's own HTTP routes. Connect/disconnect hooks and any attached `PeerRegistry` fire exactly as under `serve`.
- `Next::ctx()` and `Next::peer()`: let a cross-cutting middleware read the calling `CallContext` / `PeerHandle` that `WebSocketServer` threads through the pipeline, without the middleware being a context-aware leaf handler itself. Both return `None` on peer-less transports (TCP, direct in-process dispatch).

### Changed
- `WebSocketServer`'s reader now resolves each request (version/query validation + handler lookup) and then dispatches it inline or off-reader based on `HandlerErased::execution()`. Validation and error-response synthesis are computed once on the reader and are identical on both paths; only where the handler body runs differs. Request/response servers with no `_blocking` routes see no behavior change.
- `MiddlewarePipeline` forwards `execution()` to the wrapped handler, so registering middleware no longer downgrades a `_blocking` route back to inline.
- The `serve` / `serve_listener` / `serve_with_shutdown` / `serve_listener_with_shutdown` entry points are unified onto a single accept loop built from the public `into_shared` + `accept` + `serve_connection` primitives.

### Notes
- Off-reader dispatch is strictly opt-in; inline remains the default and keeps strict per-connection, request-at-a-time ordering. Off-reader handlers run concurrently with subsequent inline handlers and with each other, so their responses interleave on the wire — correlate responses to requests by the REPE message id (as clients already do), not by arrival order.
- A producer parked off-reader holds a process-wide `spawn_blocking` thread (tokio's default pool is 512) until its `wait_for_credit` deadline, an idle-watchdog cancel, or an `on_peer_disconnect`-driven `TransferControl::cancel`. The hold is bounded, not indefinite, but embedders expecting many concurrent streaming connections should size the runtime's `max_blocking_threads` accordingly. See `docs/websocket.md`.
- Both the saturation rejection and a caught handler panic surface as `ErrorCode::ApplicationErrorBase`. REPE defines no protocol-level "unavailable" / "internal" code today, so a client cannot tell these apart from an ordinary application error by code alone. Treat this as a known protocol gap.
- A first-class `WebSocketServer` stream-source surface (owning begin/ack/cancel/resume internally) is deferred until a second distinct windowed-transfer consumer exists, so the trait is designed against more than one shape. Until then, off-reader dispatch plus the `repe::stream` API cover the single-consumer case.

## [2.5.1] - 2026-05-20

### Changed
- Gate `PeerRegistry::id_counter` behind the `websocket` feature so default-feature builds don't warn that the method is dead code; it is only consumed by the feature-gated `websocket_server`. No API or behavior change for `websocket`-enabled builds.

## [2.5.0] - 2026-05-20

### Added
- `PeerRegistry`: cloneable live set of connected peers, with `broadcast_notify_json` / `broadcast_notify_beve` / `broadcast_notify_utf8` / `broadcast_notify_raw` helpers. Each broadcast encodes (or copies, for the pre-encoded variants) the body once on the caller's task and returns a `HashMap<PeerId, Result<(), PeerSendError>>` so callers can prune dead peers (`PeerSendError::Disconnected`) or surface backpressure (`PeerSendError::Full`). Owns its own `PeerId` allocator (`PeerRegistry::next_peer_id`), so two `WebSocketServer`s sharing one registry never mint colliding ids.
- `WebSocketServer::with_peer_registry(registry)`: attach a `PeerRegistry` so accepted peers are inserted on connect and removed on disconnect. The disconnect cleanup runs from a `Drop` guard so it fires on every exit path (clean close, transport error, handler panic).
- `WebSocketServer::on_peer_connect(f)` / `on_peer_disconnect(f)`: lower-level lifecycle hooks. Both compose: every registered closure fires in registration order, so `with_peer_registry` can coexist with logging or metrics callbacks. The connect hook runs synchronously before the reader/writer tasks start, so a notify queued from inside it is guaranteed to reach the wire before any response.
- `WebSocketServer::with_outbound_capacity(n)`: per-connection outbound channel capacity (default `DEFAULT_OUTBOUND_CAPACITY = 256`). A full channel returns `PeerSendError::Full` from `PeerHandle::send_notify`; the embedder picks the retry/prune policy.
- `Router::with_json_ctx` and `Router::with_typed_ctx`: register handlers that take a `&CallContext` alongside the body. Inside the handler, `ctx.peer()` is the calling `PeerHandle` (when the request came in over a transport that produces peers; `WebSocketServer` does), so handlers can push notifies back to the originator mid-request (progress updates, streaming chunks, server-initiated state mirroring).
- `HandlerErased::handle_with_ctx(&self, req, ctx)`: new trait method threading the `CallContext` to the leaf handler. Defaults to ignoring the context and calling `handle`, so existing implementors (embedder custom handlers) compile unchanged.
- `TypedHandlerFnCtx` in `repe::server`: trait shape mirroring `TypedHandlerFn` for `Fn(&CallContext, T) -> Result<R, ...>` closures.
- `route_request_with_ctx` (crate-internal): peer-threaded dispatch path used by `WebSocketServer`. TCP-backed servers continue to use the existing context-free `route_request`.

### Changed
- `WebSocketServer::handle_connection` is now a reader/writer task pair coordinated by a bounded `tokio::sync::mpsc` channel. The writer task is the sole point that touches the outbound `SplitSink`, so any code path that holds a `PeerHandle` can push notifies onto the wire alongside in-flight responses. Existing request-response servers see no behavior change.
- `WebSocketServer` now dispatches each request through `HandlerErased::handle_with_ctx` with a `CallContext` carrying the calling peer. Handlers registered via `with_json` / `with_typed` are unaffected (they inherit the default `handle_with_ctx` that drops the context). TCP servers continue to dispatch via the existing context-free path.

### Notes
- The push path is strictly opt-in. `WebSocketServer::new(router).serve(...)` callers compile and run unchanged. No protocol changes: notify frames are the same shape they have always been.
- Broadcast cost: one encode plus one `Vec<u8>` clone per peer. Sharing a single `Arc<[u8]>` across peers for very large broadcast bodies is left as future work.

## [2.4.0] - 2026-04-27

### Added
- `cli` feature and `repe` binary: command-line client for REPE servers. Auto-detects transport from `--url` (`ws://` / `wss://` use the WebSocket transport, anything else uses TCP, with a default port of 5099). Supports `get` / `set` / `call` / `notify` subcommands, plus an inferred mode (`repe /path` reads, `repe /path '<json>'` writes). Install with `cargo install repe --features cli`.
- CLI body sources: pass `-` as the positional body to read from stdin, or `--body-file PATH` to read from a file. The two sources are mutually exclusive with each other and with a literal positional body.
- CLI response decoding handles all body formats: JSON and BEVE responses are pretty-printed (or compacted with `--raw`), UTF-8 bodies are surfaced as JSON strings (or printed verbatim under `--raw`, so `repe --raw get /motd` behaves like a plain text fetch), and unparseable raw-binary responses are surfaced as a clear RPC error rather than an opaque decode failure.
- `REPE_URL` environment variable as a default for `--url`, so repeated invocations against the same server can drop the flag. An explicit `--url` always overrides the env var.
- `repe completions <shell>` subcommand emits a clap-derived completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`. Runs locally with no server or runtime.
- `tests/cli.rs` end-to-end coverage of the binary against an in-process registry-backed `AsyncServer` and `WebSocketServer`, exercising every subcommand, raw output, stdin/`--body-file` body sources, the body-source conflict rule, both error-exit paths, `--timeout` enforcement, BEVE response decoding, `REPE_URL` precedence, and the completions subcommand.

### Fixed
- Bare bracketed IPv6 literals in `--url` (`[::1]`) now have the default port `:5099` appended, matching the IPv4 hostname behavior. Previously they were left untouched and produced invalid socket addresses.
- `--` is now an explicit opt-out from inferred mode: `repe -- /foo` no longer rewrites to `repe -- get /foo` (which clap would then misparse), it leaves argv untouched.
- `--timeout` is now honored by `notify`. The flag was previously declared global but had no plumbing through to `Transport::notify`, so `repe --timeout 1 notify /x '{}'` silently ignored the bound.
- `--timeout` rejects negative and non-finite values (NaN, `+/-inf`) with a clear usage error instead of silently clamping to zero (which then fired immediately).
- `--body-file` is rejected for `get` before the connect attempt, so misconfigured invocations against an unreachable server surface the usage error instead of the connect failure.

### Notes
- CLI bodies are validated as syntactically-valid JSON client-side before the request is sent. Semantic validation (does this value match the schema for `/foo`?) remains the server's responsibility.

## [2.2.0] - 2026-04-25

### Added
- `repe::stream` module: backpressure-controlled streaming over REPE notifies, for protocols that push large payloads (multi-GB blobs, log streams, paginated query results) from the server to a peer and need flow control beyond what the bare notify primitive provides.
  - `TransferControl`: per-transfer state machine. ACK-driven window credit (`wait_for_credit` / `record_sent` / `record_ack`), sticky cancel signal, replay ring, peer slot, idle timestamps.
  - `TransferRegistry<K>`: typed table of in-flight transfers; the embedder picks the key type (typically a transfer-id newtype). Inbound ACK / cancel / resume handlers look the control up by key.
  - Replay ring + reconnect: `push_replay`, `replay_chunks_from`, `request_resume`, `wait_for_reconnect`, `take_pending_resume`. The producer parks on `wait_for_reconnect` after a `PeerSendError::Disconnected`; an inbound resume handler calls `request_resume(new_peer, file_index, last_received_offset)` to swap in the new peer and unpark, after which the producer replays the ring tail.
  - `spawn_watchdog<K>(registry, idle_timeout)`: background thread that scans the registry and force-cancels transfers whose last chunk and last ACK are both older than `idle_timeout`.
  - `RingChunk` carries `body_bytes: Arc<Vec<u8>>` (the exact wire body); replay is a straight resend, not a re-encode.
- The wire shape of the protocol (`transfer_begin`, `file_chunk`, ACK / cancel / resume bodies) is up to the embedder; this module deals only in offsets, ACKs, and opaque body bytes.

### Notes
- The watchdog thread holds a `Weak<TransferRegistry<K>>`, not an `Arc`. Dropping the embedder's last strong reference terminates the thread on its next tick (clamped to `[1 s, 5 s]`). For a process-wide singleton this just means the thread lives for the process; embedders that build short-lived registries (per-test, per-tenant) get clean teardown for free.
- Defaults (`DEFAULT_WINDOW_BYTES`, `DEFAULT_BACKPRESSURE_TIMEOUT`, `DEFAULT_IDLE_TIMEOUT`, `DEFAULT_REPLAY_RING_BYTES`, `DEFAULT_RECONNECT_TIMEOUT`) are tuned for LAN-class links pushing multi-GB files. Lower the window on slow links; raise the reconnect timeout for clients with long roaming windows.

## [2.1.0] - 2026-04-25

### Added
- `WebSocketClient::subscribe_notifies()` returns `Result<UnboundedReceiver<Message>, AlreadySubscribed>` and yields inbound `Message`s whose `notify` header flag is set. The receiver-side body is decoded by the application (e.g. via `Message::json_body`, `beve::from_slice`, or `MessageView`).
- `WebSocketClient::unsubscribe_notifies()` clears the active subscription so a subsequent `subscribe_notifies` call can install a new one.
- `AlreadySubscribed` error type, returned when `subscribe_notifies` is called while a live subscription already exists. Stale slots whose prior receiver was dropped do not block resubscription; the new sender installs silently.

### Changed
- `WebSocketClient`'s response loop now routes inbound messages by the notify flag *before* consulting the request/response correlation map. Server-pushed notifies that happen to share an id with an in-flight request will no longer be dispatched to the request waiter.

### Notes
- `subscribe_notifies` returns an unbounded channel by design. A bounded variant is not offered: drop-on-full corrupts chunk streams and block-on-full stalls the shared request/response path. Consumers must drain the receiver promptly; high-rate notify protocols should layer their own backpressure on top.
- `WebSocketClient` is `Clone`, and the subscription slot is shared across clones. The loud-replace contract above is what keeps two holders of the same client from silently stealing each other's subscription; if one holder needs to take over from another, it must call `unsubscribe_notifies` first.

## [2.0.0] - 2026-04-25

### Breaking changes
- `RegistryCallable::call` now takes `&CallContext` in addition to `Option<Value>`. Plain `Fn(Option<Value>) -> Result<...>` closures keep compiling unchanged via the existing blanket impl, but anyone implementing `RegistryCallable` directly via a struct must update their `fn call` signature.

### Added
- Added streaming and zero-copy `Message` I/O for large bodies:
  - `Message::write_to<W: Write>` and `Message::serialized_len` emit/size a message without allocating an intermediate frame `Vec<u8>`.
  - `write_message_streaming(w, header, query, body_len, body_writer)` lets the body be produced by a closure (pairs with `beve::to_writer_streaming` for direct BEVE-into-writer encoding of multi-MiB bodies).
  - `MessageView<'a>` borrows the query and body slices out of a caller-supplied buffer instead of copying like `Message::from_slice`. Useful with `serde_bytes::Bytes<'a>` so a chunk payload stays borrowed end-to-end.
- Peer-aware handler routing types so handlers can push notify messages back to the calling peer:
  - `PeerSink` trait that embedders implement against their per-connection outbound mechanism.
  - `PeerHandle` (cloneable wrapper over `Arc<dyn PeerSink>` plus a `PeerId`), `NotifyBody` (variant tagged with the wire `BodyFormat`), and `PeerSendError`.
  - `CallContext<'a>` carries the optional `PeerHandle` and the dispatched method through to handlers.
  - `WithContext` adapter for registering closures that want the `&CallContext`.
  - `Registry::dispatch_with_ctx` mirrors `Registry::dispatch` but threads the `CallContext` to the registered callable. `dispatch` becomes a thin wrapper that supplies a `CallContext::detached` context.
  - Built-in TCP/WebSocket servers do not yet construct `PeerHandle`s themselves; embedders that need peer routing wire their own `PeerSink` and call `dispatch_with_ctx`.

## [1.1.0] - 2026-03-12
- Added WebSocket transport support:
  - `WebSocketClient` for native async WebSocket RPC
  - `WebSocketServer` for native WebSocket serving
  - `WasmClient` for browser-based WebSocket RPC on `wasm32-unknown-unknown`
- Added `proxy_connection` support for forwarding full REPE `Message` values to an upstream `AsyncClient`.
- Added `Message::from_slice_exact` and enforced exact bounded-message length validation for WebSocket binary frames.
- Refactored server request validation/routing into shared helpers so TCP sync, TCP async, and WebSocket servers use the same request handling path.
- Refactored crate exports and target-specific dependencies so shared protocol types compile on both native and `wasm32`, while native TCP transports remain gated off the wasm target.
- Gated native integration tests to `not(target_arch = "wasm32")` so wasm-target builds do not try to compile native TCP/fleet test binaries.

## [1.0.0] - 2026-03-05
- Added multi-node fleet APIs:
  - `Fleet` for synchronous TCP request/response fanout
  - `AsyncFleet` for asynchronous tokio-based TCP fanout
  - `UniUdpFleet` for unidirectional UDP fanout
- Added shared fleet types and configuration:
  - `NodeConfig`, `FleetOptions`, `RetryPolicy`
  - `RemoteResult`, `HealthStatus`
  - connect/disconnect/reconnect summary types
- Added fleet operations:
  - connection lifecycle (`connect_all`, `disconnect_all`, `reconnect_disconnected`)
  - single-node calls (`call_json`, `call_message`)
  - tag-filtered broadcast (`broadcast_json`) and reduction (`map_reduce_json`)
  - per-node health checks (`health_check`)
- Updated TCP fleet retry policy to retry only transport/I/O failures and stop retrying on application-level server errors.
- Added UDP foundations:
  - `UniUdpClient` with notify/request send APIs and per-message IDs, backed by the `uniudp` crate
  - UDP node config with redundancy/chunk/FEC fields
  - UniUDP default RS profile now uses `fec_group_size=4` with `parity_shards=2`
- Made UniUDP support opt-in behind the `fleet-udp` Cargo feature so TCP-only builds avoid UniUDP dependencies.
- Added integration tests for:
  - sync fleet behavior (`tests/fleet_tests.rs`)
  - async fleet behavior (`tests/async_fleet_tests.rs`)
  - UDP fleet behavior (`tests/uniudp_fleet_tests.rs`)
- Added fleet documentation (`docs/fleet.md`) and README examples.

## [0.4.2] - 2026-02-22
- Added multiplexed request handling to `Client` and `AsyncClient` so multiple in-flight calls can share a single connection and still match responses by request ID.
- Added per-request timeout helpers on both clients:
  - `call_json_with_timeout`
  - `call_typed_json_with_timeout`
  - `call_typed_beve_with_timeout`
- Added JSON batch helpers on both clients:
  - `batch_json`
  - `batch_json_with_timeout`
- Hardened unknown-response-ID handling:
  - unknown response IDs are now logged and dropped by default
  - late responses for timed-out requests are also dropped without tearing down the connection
- Made `AsyncClient` request tracking cancellation-safe so dropped call futures do not leak entries in the pending-request map.
- Preserved structured fatal response-loop errors when failing pending requests instead of flattening everything to `Io(ConnectionAborted)`.
- Bounded sync `Client::batch_json` worker threads to avoid unbounded OS thread creation on large batches.

## [0.4.0] - 2025-10-24
- Router middleware hooks (`with_middleware` / `register_middleware`) let servers centralize auth, logging, or validation without manually wrapping each handler.
- Router shared-struct registration now accepts any `Lockable` lock, including `tokio::sync`
  mutexes/RwLocks out of the box and `parking_lot` locks when the optional feature is enabled.
- Bumped the edition to Rust 2024 and raised the MSRV to 1.85.

## [0.2.0] - 2025-09-18
- Added full BEVE body support (builder helpers, response serialization, and message decoding) backed by the official `beve` crate.
- Server routers and typed handlers now accept BEVE payloads, mirroring existing JSON ergonomics.
- Documented BEVE usage and updated the spec reference URL; expanded tests to cover complex BEVE round-trips.

## [0.1.3] - 2025-09-17
- Added zero-copy query routing so servers reuse borrowed UTF-8 query slices, cutting per-request allocations and tightening error handling for invalid query encodings.

## [0.1.2] - 2025-09-17
- Stream sync and async REPE message I/O directly into final buffers to avoid extra allocations and copies.
- Reuse persistent buffered readers/writers in the TCP client to eliminate per-request socket clones.
