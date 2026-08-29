# repe-rs

Rust implementation of the [REPE RPC protocol](https://github.com/repe-org/REPE) with JSON and BEVE body support.

[![crates.io](https://img.shields.io/crates/v/repe.svg)](https://crates.io/crates/repe)

## What's in the box

- REPE header and message types with correct little-endian wire encoding.
- Streaming and zero-copy I/O (`MessageView`, `write_message_streaming`) for large bodies.
- JSON bodies via `serde_json`; BEVE bodies via the [`beve`](https://crates.io/crates/beve) crate.
- A bulk fast path for [high-throughput numeric bodies](numeric-bodies.md) (typed numeric and complex arrays) that encodes, decodes, and frames a whole-body slice in a single copy.
- Sync and async (tokio) clients and servers, with multiplexed in-flight requests, per-call timeouts, batching, and notify support.
- Dynamic [`Registry`](registry.md) routing with JSON Pointer semantics.
- [`Fleet`](fleet.md) APIs for multi-node TCP and UDP fanout.
- Optional [REST gateway](rest.md) (`RestGateway`) fronting a `Registry` over HTTP/1.1 and HTTP/2, with verb-checked reads/writes/calls, `ETag` revalidation, and JSON/BEVE negotiation.
- Optional [WebSocket transport](websocket.md), including a wasm browser client and server-pushed notify subscriptions.
- Optional [`stream`](streaming.md) module for backpressure-controlled bulk transfers with reconnect-resume.
- Optional [`repe` CLI](cli.md) for talking to any REPE server over TCP or WebSocket.
- [Wire compatibility](interop.md) with the canonical C++ implementation (Glaze), pinned by an automated interop fixture suite.

## Install

```toml
[dependencies]
repe = "12"
```

Or `cargo add repe`. The CLI ships under a feature flag: `cargo install repe --features cli`.

## Quick start

```rust
use repe::{BodyFormat, Message, QueryFormat};

let msg = Message::builder()
    .id(42)
    .query_str("/status")
    .query_format(QueryFormat::JsonPointer)
    .body_json(&serde_json::json!({"ping": true}))?
    .build();

let bytes = msg.to_vec();
let parsed = repe::Message::from_slice(&bytes)?;

assert_eq!(parsed.header.id, 42);
assert_eq!(parsed.header.body_format, BodyFormat::Json as u16);
let val: serde_json::Value = parsed.json_body()?;
assert_eq!(val["ping"], true);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Replace `body_json` with `body_beve` to encode the body with BEVE; everything else is identical.

## Where to next

- New here? Start with the [wire format](protocol.md) for a quick mental model, then read the [server](server.md) and [client](client.md) pages.
- Building dynamic routes? See [Registry](registry.md).
- Multi-node deployments? See [Fleet](fleet.md).
- Exposing a registry to public clients or a CDN? See [REST gateway](rest.md).
- Pushing through firewalls or browsers? See [WebSocket](websocket.md).
- Moving large payloads with flow control? See [Streaming](streaming.md).
- Debugging from a terminal? See the [CLI](cli.md).
- Interoperating with C++ (Glaze) or another REPE implementation? See [Interoperability](interop.md).

## Feature flags

| Flag | Effect |
| --- | --- |
| `rest` | `RestGateway`: a REST facade over a `Registry`, served on HTTP/1.1 and HTTP/2. |
| `websocket` | Native `WebSocketClient`, `WebSocketServer`, and `proxy_connection`. |
| `websocket-wasm` | Browser `WasmClient` on `wasm32-unknown-unknown`. |
| `fleet-udp` | UDP fanout via `UniUdpFleet`. |
| `value-stream` | Streaming value transfer (SVS), with an optional Zstandard codec (pure Rust, no C toolchain). |
| `parking-lot` | `Lockable` impls for `parking_lot::Mutex` / `RwLock`. |
| `cli` | Builds the `repe` command-line client (pulls in `clap` and `websocket`). |

`fleet-udp`, `value-stream`, and `rest` are native-only: on `wasm32-unknown-unknown` their modules and their non-portable dependencies both compile away, so enabling any of them there is inert rather than a build failure. That matters for a workspace hoisting one shared feature set across a native server and a browser client, since Cargo features are additive and the wasm member cannot subtract one.

## License

MIT, see [`LICENSE`](https://github.com/repe-org/repe-rs/blob/main/LICENSE) in the repo.
