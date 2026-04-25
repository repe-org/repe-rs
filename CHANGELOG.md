# Changelog

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
