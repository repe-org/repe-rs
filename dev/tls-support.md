# TLS Support

## Status

Not started. This is an investigation and a proposed design, not a shipped record.

The immediate trigger: `wss://` is advertised in the CLI help and in `docs/cli.md`, and it does not work. Everything past that is the question of how to support TLS properly rather than bolt it on.

## Summary

repe has no TLS support on any native transport. Adding it is not one feature flag, because three things in the current shape assume a plain `TcpStream`:

1. The WebSocket server API is concrete over `TcpStream` in every public signature.
2. Two of the four clients split a connection using mechanisms that exist only for TCP sockets (`try_clone`, `into_split`). Neither works for a TLS session.
3. The shipped co-hosting helper `is_websocket_upgrade` is built on `TcpStream::peek`, and a decrypted TLS stream has no `peek`.

The proposal is three phases, ordered by value per unit of work:

- **Phase 1: the WebSocket transport.** Cheapest and highest value. Its split is already lock-based rather than fd-based, one internal function is already generic over the stream, and it is the transport that browsers force TLS on. Fully additive.
- **Phase 2: co-hosting over TLS.** ALPN plus a buffering `Peekable` adapter, so the peek-then-fork recipe survives.
- **Phase 3: the raw TCP transports.** The sync client needs restructuring, not retyping. Worth deferring until someone asks.

## Motivation

Browsers gate a growing set of APIs on the page being a *secure context*, which is determined by origin scheme. A page served over `http:` from anything other than `localhost` loses the clipboard API, and more besides. Any project that serves a wasm REPE client from a non-loopback address therefore needs `https:` for the page, and once the page is `https:`, the browser's mixed-content rule requires `wss:` for the socket. At that point repe is the component that cannot comply.

The workaround is a TLS-terminating reverse proxy, which is a legitimate answer and will remain the right answer for many deployments. It is not a complete one: it requires a second process, it moves the trust configuration out of the application, and it does nothing for the native `Client` / `AsyncClient` talking to a server across an untrusted network.

There is also a correctness argument independent of any deployment. The crate currently advertises a capability it does not have.

## Current state

### `wss://` is advertised and does not work

`src/bin/repe.rs:53` documents `wss://host:port[/path]` in the `--url` help, `:518` routes it to the WebSocket transport, and `:678` has a unit test asserting the scheme is detected. `docs/cli.md:11` repeats the claim.

`Cargo.toml` declares `tokio-tungstenite = { version = "0.24", optional = true }` with default features, which are `["connect", "handshake"]`. Neither pulls a TLS backend, so:

```
$ cargo tree --features websocket | grep -i tls
(nothing)
```

and the runtime result:

```
$ cargo run --features cli --bin repe -- --url wss://example.com:443/ get /
websocket connect to wss://example.com:443/ failed: URL error: TLS support not compiled in
```

`src/websocket_client.rs:24` names `MaybeTlsStream<TcpStream>` in its type aliases, which reads as TLS support in the source and in rustdoc. Without a backend feature that enum only ever holds its `Plain` variant.

Fixing just this claim is a one-line Cargo change plus a feature flag. It is worth separating from the rest: it is the only part that is a bug rather than a missing feature.

### Per-transport inventory

| Transport | Module | TLS today | How it splits the connection | Cost to add TLS |
|---|---|---|---|---|
| wasm WebSocket client | `wasm_client.rs` | **Works** | n/a, browser owns the socket | none |
| WebSocket client | `websocket_client.rs` | No | `futures_util` `split()` (BiLock) | low |
| WebSocket server | `websocket_server.rs` | No | `futures_util` `split()` (BiLock) | low, but 28 `TcpStream` mentions |
| Sync TCP server | `server.rs` | No | one thread owns the stream, sequential | low |
| Async TCP server | `async_server.rs` | No | `TcpStream::into_split` | low, split is not actually needed |
| Async TCP client | `async_client.rs` | No | `TcpStream::into_split` | moderate |
| Sync TCP client | `client.rs` | No | `TcpStream::try_clone` | **high, needs restructuring** |
| UDP fleet | `udp_client.rs`, `uniudp_fleet.rs` | No | n/a | out of scope, see below |

The wasm client is worth calling out: `WebSocket::new(url)` at `src/wasm_client.rs:99` hands the URL to the browser, so `wss://` already works there with no repe code at all. It is the only transport that is already correct.

The codec layer is already transport-agnostic and needs no work: `io.rs` is generic over `Read`/`Write` and `async_io.rs` over `AsyncRead`/`AsyncWrite`. TLS is a connection-establishment and API-surface problem, not a framing problem.

## Why this is not just a feature flag

### 1. The server API is concrete over `TcpStream`

Every public entry point on the WebSocket server names the stream type:

```rust
pub async fn accept(stream: TcpStream, ...) -> Result<WebSocketStream<TcpStream>, RepeError>          // :844
pub async fn serve_connection(&self, ws: WebSocketStream<TcpStream>) -> Result<(), RepeError>          // :982
```

An embedder holding a `tokio_rustls::server::TlsStream<TcpStream>` has nothing to call.

There is already precedent in the file for the fix. `proxy_connection` (`:1098`) is generic over exactly the right bound:

```rust
pub async fn proxy_connection<S>(ws_stream: WebSocketStream<S>, upstream: AsyncClient) -> Result<(), RepeError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
```

So the bound is written and proven; it just is not applied to the main path. `handle_connection_with_config` (`:1194`) and the `writer_task` / `reader_task` pair (`:1554`, `:1318`) would need the same parameter threaded through.

### 2. Two clients split the connection with TCP-only mechanisms

This is the part that makes the raw TCP transports genuinely expensive, and it is invisible from the type signatures.

`Client::connect` (`src/client.rs:68`) does:

```rust
let reader_stream = stream.try_clone()?;
```

`try_clone` duplicates the file descriptor. Two independent handles then read and write the same TCP connection concurrently, which is safe because the kernel serializes at the socket. A TLS session cannot do this. It has one record layer with one sequence number and one cipher state, held in a single object. There is no `try_clone` for it and there cannot be.

`AsyncClient::connect` (`src/async_client.rs:103`) has the same shape via `stream.into_split()`, which is a `TcpStream` inherent method producing `OwnedReadHalf` / `OwnedWriteHalf`.

The async case has a clean answer: `tokio::io::split()` is generic and works on any `AsyncRead + AsyncWrite`, at the cost of an internal lock per operation. `async_server.rs:67` does not even need that, since its loop is sequential and could simply keep the whole stream.

The sync case does not have a clean answer. The options are:

- Wrap the TLS stream in a `Mutex` and let the reader thread lock it. Wrong: the reader blocks on `read` while holding the lock, so writers starve for as long as the connection is idle.
- Split at the record layer. Read raw ciphertext from the TCP socket with no lock held, then take the lock only to feed `read_tls` / `process_new_packets` and to drain `writer()` / `write_tls`. This works and is what a correct implementation looks like, but it is a rewrite of `ClientInner`'s I/O strategy, not a type change.
- Move to a single I/O thread with an outbound queue, which is closer to how `websocket_client` already works.

Whichever is chosen, `Client` is a redesign. This is the main argument for phasing.

By contrast, both WebSocket paths already split with `futures_util`'s `StreamExt::split()` (`websocket_client.rs:165`, `websocket_server.rs:1224`), which is BiLock-based and completely indifferent to the underlying stream type. **The WebSocket transport pays nothing structurally to gain TLS.** That is the core reason to do it first.

### 3. The co-hosting helper is built on `MSG_PEEK`

`is_websocket_upgrade` (`src/websocket_server.rs:1734`) takes `&TcpStream` and uses `peek`, the whole point being that it inspects the first bytes without consuming them so an HTTP handler can still read the full request. This is the recipe blessed in `http-cohosting-and-keyed-peer-registry.md` and shipped in 3.2.0.

`peek` is a socket syscall. A decrypted TLS stream has no equivalent, and peeking the raw socket returns a TLS ClientHello, which tells you nothing about what is inside. Co-hosting over TLS needs a different mechanism, covered in Phase 2.

## Design

### Feature flags

Follow `tokio-tungstenite`'s split so the backend choice passes straight through and repe does not invent a third vocabulary:

```toml
tls-rustls        = ["websocket", "dep:tokio-rustls", "tokio-tungstenite/rustls-tls-native-roots"]
tls-rustls-webpki = ["websocket", "dep:tokio-rustls", "tokio-tungstenite/rustls-tls-webpki-roots"]
tls-native        = ["websocket", "dep:tokio-native-tls", "tokio-tungstenite/native-tls"]
```

Notes on the choice, which should be the user's and not the crate's:

- **rustls** avoids an OpenSSL dependency and cross-compiles cleanly. It does not read the OS trust store unless `rustls-native-certs` is pulled in, which is why the native-roots and webpki-roots variants are separate flags. An instrument on a private CA wants native roots; a client talking to public endpoints from a container may prefer webpki-roots.
- **native-tls** uses SChannel, Security.framework, and OpenSSL respectively, so it inherits the platform trust store and the platform's policy for free, at the cost of a C dependency on Linux.
- Pin down which rustls crypto provider ends up selected before committing. rustls 0.23 requires one, and the default pulls a build-time toolchain requirement that some consumers will not want. If it is not `ring`, consider whether to re-export the choice.

Default stays off. Enabling TLS by default would add a large dependency tree to a crate whose current selling point is a lean one.

### Phase 1: the WebSocket transport

**1a. Genericize the internals. No API change, ships on its own.**

Thread `S: AsyncRead + AsyncWrite + Unpin + Send + 'static` through `handle_connection_with_config`, `reader_task`, `writer_task`, and `accept_repe_websocket`. This is mechanical and has zero public effect, since the existing concrete entry points call them with `S = TcpStream`. Doing it as its own commit keeps the diff reviewable.

**1b. Client: make `wss://` work.**

With a backend feature enabled, `connect_async_with_config` handles `wss://` with no repe code change, because `MaybeTlsStream` already covers both cases. The work is the feature wiring plus a connect-time configuration surface:

```rust
WebSocketClient::connect_with_tls(url, TlsClientConfig)
```

where `TlsClientConfig` offers: default platform roots; additional roots from PEM; a client certificate for mTLS; and an escape hatch for accepting an unverified certificate (see below). It should also accept a prebuilt `Arc<rustls::ClientConfig>`, because anything the builder does not cover (session resumption, ALPN, custom verifiers, key logging for debugging) has to remain reachable without a repe release.

**1c. Server: accept an already-upgraded TLS stream, and offer a turnkey listener.**

Two levels, additive:

```rust
// Level 1: the embedder owns TLS. Additive generic siblings of the existing methods.
pub async fn accept_tls<S>(stream: S, path: &str) -> Result<WebSocketStream<S>, RepeError>
pub async fn serve_connection_on<S>(&self, ws: WebSocketStream<S>) -> Result<(), RepeError>

// Level 2: turnkey.
pub async fn serve_tls<A: ToSocketAddrs>(self, addr: A, path: &str, tls: TlsServerConfig) -> std::io::Result<()>
```

Level 1 is what an embedder with its own accept loop needs, and it is the one that must exist. Level 2 is the common case and should not require anyone to learn `tokio-rustls`.

`TlsServerConfig` should have a `from_pem_files(cert_chain, key)` constructor for the 90% case and a `From<Arc<rustls::ServerConfig>>` for everything else (SNI with multiple certificates, OCSP stapling, session ticket configuration, ALPN).

### Phase 2: co-hosting over TLS

Two complementary answers, and both are worth having:

**ALPN, which removes the need to peek at all.** Advertise `http/1.1` and a REPE protocol identifier at the handshake, then dispatch on the negotiated protocol. This is strictly better than byte-sniffing when both ends support it: the classification is done before any application byte moves, and it cannot be fooled. Expose the negotiated value on the accepted connection so the embedder can branch on it.

**A buffering `Peekable<S>` adapter, for when ALPN is not available.** Browsers do not let a page choose an ALPN value for a WebSocket, so the negotiated protocol will often be `http/1.1` for both the upgrade and the plain HTTP routes. The generic replacement for `MSG_PEEK` is an adapter that reads into an internal buffer and replays those bytes to the next reader:

```rust
pub struct Peekable<S> { inner: S, buf: Vec<u8>, pos: usize }
impl<S: AsyncRead + AsyncWrite + Unpin> Peekable<S> {
    pub async fn peek(&mut self, n: usize) -> std::io::Result<&[u8]>;
}
impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Peekable<S> { /* drains buf first */ }
```

`is_websocket_upgrade` then gets a generic sibling taking `&mut Peekable<S>`, and the existing `TcpStream` version stays as it is. The example in `examples/websocket_cohosting.rs` should gain a TLS variant, since the peek-then-fork shape is exactly where an embedder will get this wrong.

### Phase 3: raw TCP transports

Deferred, and honestly the case for it is weaker: a native client on an untrusted network can be tunnelled, and this phase costs the most. When it happens:

- `async_server`: replace `into_split` with keeping the stream whole, since the loop is sequential. Nearly free.
- `server` (sync): a single thread already owns the stream sequentially, so `rustls::StreamOwned` drops in. Nearly free.
- `async_client`: `tokio::io::split()` in place of `into_split`. Straightforward, slight per-operation lock cost.
- `client` (sync): the restructuring described above. This is the one that needs a design of its own.

Genericizing `Client`/`AsyncClient` over the stream also means the struct definitions gain a type parameter (`ClientInner` holds `Mutex<BufWriter<TcpStream>>` at `src/client.rs:30`), which is a public-facing change in a way the WebSocket work is not. An alternative worth weighing is a separate `TlsClient` type rather than a parameterized `Client`, trading duplication for leaving the existing type alone.

### mTLS

Client certificates should be in scope from the start on both sides, because retrofitting them changes the config types. Server side: an optional client-cert verifier with a root store. Client side: a certificate and key in `TlsClientConfig`. This is the one authentication mechanism that fits repe's current design without inventing a protocol-level auth concept, since it lives entirely under the transport.

Where the verified peer identity surfaces matters. `HandshakeContext` (already threaded to `on_peer_connect_with_handshake`) is the natural home for the peer certificate subject, so a handler can authorize on it.

### The insecure escape hatch

Provide one, deliberately and visibly:

```rust
#[cfg(feature = "tls-dangerous")]
pub fn dangerous_accept_any_certificate(self) -> Self
```

Behind its own feature, with `dangerous` in both the feature name and the method name. The reasoning is that everyone developing against a self-signed certificate needs this, and if the crate does not provide it they will hand-write a `ServerCertVerifier` that returns `Ok` unconditionally, which is both more dangerous and invisible in review. A greppable name that cannot be enabled by accident is the safer outcome. It should log a warning on use.

### Out of scope

**UDP and the fleet transports.** DTLS is a different protocol with a materially worse Rust story, and the UDP fleet is a fanout mechanism for trusted networks. Explicitly not planned; revisit only if asked.

**The wasm client.** Already correct, nothing to do.

**The C++ interop suite.** `interop/` exercises the wire format over plain TCP. TLS is beneath the frame, so it cannot change interop results, and adding TLS to the harness would mean a TLS stack on the Glaze side for no protocol coverage.

## Semver and compatibility

The additive route keeps this a minor release:

- New generic methods (`accept_tls`, `serve_connection_on`) alongside the existing concrete ones, with the concrete ones becoming thin wrappers.
- Genericizing private functions has no semver effect.

Changing `accept` itself from `stream: TcpStream` to `stream: S` looks source-compatible, since inference handles a `TcpStream` argument, but it is not: any embedder that *names* the return type, for example storing a `WebSocketStream<TcpStream>` in a struct field or writing it in a function signature, stops compiling. That is a realistic pattern for someone running their own accept loop. So the concrete signatures should stay until a major bump, then collapse into the generic ones.

Adding a TLS feature does not affect default builds. The one visible change for existing users is that `wss://` starts working instead of erroring, which is a bug fix.

## Testing

- Generate a test CA plus a leaf certificate at test time with `rcgen` as a dev-dependency. Do not commit fixture certificates: they expire, and a test suite that fails on a date is worse than no test.
- Round trips per backend feature: client trusts the test CA and succeeds; client without the CA fails with a certificate error rather than hanging; hostname mismatch is rejected; mTLS succeeds both ways and a client with no certificate is refused when one is required.
- One test that the `dangerous` opt-in actually accepts an untrusted certificate, so the escape hatch cannot silently stop working.
- A co-hosting test over TLS covering both branches through `Peekable`, plus one asserting ALPN dispatch.
- CI needs a matrix entry per backend. `tls-rustls` and `tls-native` will not be exercised by a single build.

## Work estimate

Rough, and the ordering matters more than the numbers:

| Item | Size |
|---|---|
| Fix the `wss://` claim (feature wiring, or correct the docs) | hours |
| Phase 1a, genericize internals | hours |
| Phase 1b + 1c, client and server TLS surfaces plus config types | days |
| Phase 2, ALPN and `Peekable` co-hosting | 1 to 2 days |
| Phase 3, raw TCP, dominated by the sync `Client` restructure | its own design cycle |

The `wss://` fix should not wait on any of the rest.

## Open questions

1. Should the CLI's `wss://` support ship immediately as a doc correction (state that TLS is not compiled in) or wait for Phase 1b? Shipping a correction first is honest and cheap; shipping the feature makes the correction unnecessary.
2. Is a parameterized `Client<S>` or a separate `TlsClient` the better shape for Phase 3? The answer probably follows from whether anyone actually wants raw-TCP TLS.
3. Does the crate want an opinion on ALPN protocol identifiers for REPE, or is that a question for the REPE specification rather than this implementation?
4. Should `TlsServerConfig` support hot certificate reload? Long-lived servers with short-lived certificates need it, and retrofitting it means an indirection in the accept path that is cheap to add now and awkward later.
