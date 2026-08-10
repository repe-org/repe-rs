# Browser tests for `WasmClient`

`src/wasm_client.rs` is gated on `target_arch = "wasm32"`, so nothing in the host test suite compiles it, let alone runs it. CI proves it builds (`cargo clippy --target wasm32-unknown-unknown`); these tests prove it works.

That gap is not theoretical. The module failed to compile for `wasm32` across four releases (6.0.0 through 7.0.1) without anyone noticing, because `--all-features` on a host runner leaves the cfg false. 7.0.2 closed the compile gap. What remained was the harder failure mode: code that builds and silently does nothing, which is exactly how server-pushed notifies went unreachable from the browser.

Pure logic is better hoisted into target-agnostic modules and unit-tested on the host — `src/notify_slot.rs` is the pattern. These tests exist for what cannot be hoisted: the JS callback wiring, `web_sys::WebSocket` itself, and the ordering between them. Both bugs this suite would have caught lived in exactly that glue.

## Layout

Both are standalone crates, outside the published package: `wasm-tests/` is absent from the `include` list in `Cargo.toml`, like `interop/`.

- `server/` — a scripted REPE WebSocket server. Not a `repe::WebSocketServer`: the tests need byte-level control the peer API deliberately does not expose (a notify whose id collides with an in-flight request, a deliberately undecodable frame, a server-initiated close).
- `client/` — the `wasm-bindgen-test` suite, driving `repe`'s public API.

The client is its own crate rather than a file in `tests/` because `wasm-pack test` always passes `--tests`. Run from the repository root it would build all two dozen host-only integration tests for `wasm32` and fail on the first `#[cfg(not(target_arch = "wasm32"))]` item they import. From here it sees only the browser suite.

## Running locally

Needs [`wasm-pack`](https://rustwasm.github.io/wasm-pack/) and Chrome.

```sh
cargo run --manifest-path wasm-tests/server/Cargo.toml &
(cd wasm-tests/client && wasm-pack test --headless --chrome)
kill %1
```

The server listens on `127.0.0.1:8791`. Override with `REPE_WASM_TEST_PORT` for the server and `REPE_WASM_TEST_URL` (read at compile time) for the tests.

Swap `--chrome` for `--firefox` if you would rather not have Chrome; the suite uses nothing browser-specific.

## Adding a scenario

Add a route to `server/src/main.rs` and a test that calls it. Keep the server driven by the client: a scenario that depends on a sleep is a scenario that will flake in CI. Every route here reacts to a request, and every ordering the tests assert is a wire ordering the WebSocket protocol guarantees.
