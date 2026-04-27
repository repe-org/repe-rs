# repe-rs

Rust implementation of the [REPE RPC protocol](https://github.com/repe-org/REPE) with JSON and BEVE body support.

[![crates.io](https://img.shields.io/crates/v/repe.svg)](https://crates.io/crates/repe)
[![docs](https://img.shields.io/badge/docs-repe--rs-blue?logo=readthedocs&logoColor=white)](https://repe-org.github.io/repe-rs/)

## Features

- REPE header and message types with correct little-endian wire encoding.
- Streaming and zero-copy I/O (`MessageView`, `write_message_streaming`) for large bodies.
- JSON bodies via `serde_json`; BEVE bodies via the [`beve`](https://crates.io/crates/beve) crate.
- Sync and async (tokio) clients and servers, with multiplexed in-flight requests, per-call timeouts, batching, and notify support.
- Dynamic [`Registry`](https://repe-org.github.io/repe-rs/registry/) routing with JSON Pointer semantics.
- [`Fleet`](https://repe-org.github.io/repe-rs/fleet/) APIs for multi-node TCP and UDP fanout.
- Optional [WebSocket transport](https://repe-org.github.io/repe-rs/websocket/), including a wasm browser client and server-pushed notify subscriptions.
- Optional [`stream`](https://repe-org.github.io/repe-rs/streaming/) module for backpressure-controlled bulk transfers with reconnect-resume.
- Optional [`repe` CLI](https://repe-org.github.io/repe-rs/cli/) for talking to any REPE server over TCP or WebSocket.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
repe = "2.3"
```

Or run `cargo add repe`.

## Quick Start

Build, serialize, and parse a JSON message:

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

## Server and Client

A minimal TCP server with one route, plus a client call:

```rust
use repe::{Client, Router, Server};
use serde_json::json;

let router = Router::new().with("/ping", |_v| Ok(json!({"pong": true})));
let server = Server::new(router);
let listener = server.listen("127.0.0.1:0")?;
let addr = listener.local_addr()?;
std::thread::spawn(move || { let _ = server.serve(listener); });

let client = Client::connect(addr)?;
let pong = client.call_json("/ping", &json!({}))?;
assert_eq!(pong["pong"], true);
# Ok::<(), Box<dyn std::error::Error>>(())
```

The async (`tokio`) variant uses `AsyncServer` and `AsyncClient` and exposes the same shape. See the [Server guide](https://repe-org.github.io/repe-rs/server/) for routers, typed handlers, middleware, struct registration, and peer-aware handlers, and the [Client guide](https://repe-org.github.io/repe-rs/client/) for the full client surface (typed and BEVE helpers, multiplexing, timeouts, batches, notifies, error handling).

## Feature Flags

| Flag | Effect |
| --- | --- |
| `websocket` | Native `WebSocketClient`, `WebSocketServer`, and `proxy_connection`. |
| `websocket-wasm` | Browser `WasmClient` on `wasm32-unknown-unknown`. |
| `fleet-udp` | UDP fanout via `UniUdpFleet`. |
| `parking-lot` | `Lockable` impls for `parking_lot::Mutex` / `RwLock`. |
| `cli` | Builds the `repe` command-line client (pulls in `clap` and `websocket`). |

## Documentation

Full documentation is hosted at **[repe-org.github.io/repe-rs](https://repe-org.github.io/repe-rs/)**.

- [Server, routers, and handlers](https://repe-org.github.io/repe-rs/server/)
- [Client APIs](https://repe-org.github.io/repe-rs/client/)
- [Registry (dynamic routing)](https://repe-org.github.io/repe-rs/registry/)
- [Fleet (multi-node control)](https://repe-org.github.io/repe-rs/fleet/)
- [WebSocket transport](https://repe-org.github.io/repe-rs/websocket/)
- [Streaming with backpressure](https://repe-org.github.io/repe-rs/streaming/)
- [Command-line client](https://repe-org.github.io/repe-rs/cli/)
- [Wire format and JSON Pointer helpers](https://repe-org.github.io/repe-rs/protocol/)

## Examples

```
cargo run --example json_roundtrip
cargo run --example server
cargo run --example client
cargo run --example registry_server
cargo run --example registry_roundtrip
cargo run --example async_server
cargo run --example async_client
```

## Testing

Run `cargo test` to execute unit and integration tests. The crate denies warnings and includes async tests, so a recent tokio is required. Integration tests cover sync and async server/client calls, unknown routes, handler error mapping, ID mismatches, and timeouts.

## License

MIT, see `LICENSE`.
