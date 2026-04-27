# Changelog

## [Unreleased]

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
