# REPE C++ interop

This directory pins compatibility between `repe` (this crate) and the canonical
C++ REPE implementation in [Glaze](https://github.com/stephenberry/glaze), in two
halves against one pinned Glaze tag:

- **Wire format** — committed fixture frames produced by Glaze, parsed by Rust.
- **Plugin ABI, both directions** — a C++ Glaze host that `dlopen`s a Rust plugin and drives it, and a Rust host that `dlopen`s a C++ Glaze plugin and drives it.

Both implement REPE **version 1** (the 48-byte header). See
[`docs/interop.md`](../docs/interop.md) for the compatibility guarantee and the
known v1-vs-v2 spec divergence, and [`docs/plugins.md`](../docs/plugins.md) for
the plugin ABI.

## Layout

- `cpp/generate_fixtures.cpp` — a small program that links Glaze and emits
  authentic REPE frames (it is the *only* source of the bytes; nothing here is
  hand-authored).
- `cpp/plugin_host.cpp` — a C++ REPE host that `dlopen`s a Rust plugin and
  drives it through `glaze/rpc/repe/plugin.h`. Every frame it sends is encoded by
  Glaze and every response is decoded by Glaze, so a framing divergence surfaces
  as a failed decode rather than as a byte comparison both sides got equally
  wrong.
- `cpp/example_plugin.cpp` — the mirror: a C++ Glaze plugin, built as a shared
  library, for the Rust host in `examples/plugin_host.rs` to load. It publishes
  the same object as `examples/repe_plugin.rs`, field for field and method for
  method, so one host binary drives both implementations with the same
  expectations.
- `cpp/CMakeLists.txt` — builds all three; fetches Glaze at a pinned tag by
  default.
- `fixtures/*.repe` — committed raw REPE frames produced by the generator.
- `fixtures/manifest.json` — committed; describes the expected decode of each
  frame. Consumed by `tests/interop.rs`.

The C++ generator and the fixtures are **dev-only**: `interop/` is excluded from
the published crate (it is not in `Cargo.toml`'s `include` list). The Rust tests
in `tests/interop.rs` read `fixtures/` at runtime.

## Regenerating the fixtures

Requires CMake and a C++23 compiler. By default Glaze is fetched at the pinned
tag (`v8.0.0`) so the committed bytes and CI stay in lockstep:

```sh
cmake -S interop/cpp -B interop/cpp/build
cmake --build interop/cpp/build
interop/cpp/build/generate_fixtures interop/fixtures v8.0.0
```

To iterate against a local Glaze checkout instead of fetching:

```sh
cmake -S interop/cpp -B interop/cpp/build -DGLAZE_SOURCE_DIR=/path/to/glaze
cmake --build interop/cpp/build
interop/cpp/build/generate_fixtures interop/fixtures
```

Then verify nothing drifted and the Rust side still agrees:

```sh
git diff --exit-code -- interop/fixtures   # committed bytes match current Glaze
cargo test --test interop
```

The `interop` CI workflow runs exactly this loop, so a fixture that no longer
matches the pinned Glaze output (or a repe-rs change that breaks parity) fails
the build.

## Running the plugin-ABI checks

This is the half no Rust-only test can cover: whether each implementation's
plugins and hosts actually work against the *other's*. One implementation
driving itself agrees with itself by construction — both ends can misread the
same clause of `plugin.h` and pass — so it is run in both directions.

### A C++ host driving a Rust plugin

Build the plugin as a shared library, then point the host at it:

```sh
cargo build --release --features plugin --example repe_plugin
cmake -S interop/cpp -B interop/cpp/build && cmake --build interop/cpp/build
# .so on Linux, .dylib on macOS
interop/cpp/build/plugin_host target/release/examples/librepe_plugin.dylib
```

It prints one line per expectation and exits non-zero on any failure. What it
pins, none of which `tests/plugin_abi.rs` can reach from inside Rust:

- the five exported symbols resolve by name, with the signatures in `plugin.h`;
- `repe_plugin_data`'s layout, read through Glaze's own declaration;
- the version handshake, checked before the metadata struct is read;
- the response-buffer contract, including a `std::string_view` actually
  constructed from a zero-size response — undefined behavior if `data` were null;
- field reads and writes, methods with and without arguments, a handler `Err`
  arriving as an error frame, and a `#[repe(typed)]` field crossing as BEVE,
  each decoded by Glaze into a C++ type;
- notify producing no response, an unknown method producing `method_not_found`,
  a malformed frame producing an id-0 error, and a post-shutdown call being
  refused.

### A Rust host driving a C++ plugin

The mirror, and the same CMake build produces the plugin:

```sh
cargo build --release --features plugin-host --example plugin_host
cmake -S interop/cpp -B interop/cpp/build && cmake --build interop/cpp/build
target/release/examples/plugin_host interop/cpp/build/libexample_plugin.so
```

What this pins that the direction above cannot:

- `dlopen` and `dlsym` against a library this crate did not produce, so the
  symbol names and signatures are checked against Glaze's exports rather than
  against the ones `#[repe::plugin]` emits;
- `repe_plugin_data` read from a C++ `static`, so the layout is checked against
  a struct laid out by the C++ compiler;
- a response buffer owned by Glaze's thread-local `std::string`, copied out by
  the host before the borrow expires;
- both optional lifecycle symbols present, which is the branch a plugin that
  omits them cannot exercise.

The same host binary drives the Rust plugin too, which is worth doing while
editing either of them — though CI does not, since that pairing is one
implementation agreeing with itself and `tests/plugin_host.rs` already covers
it:

```sh
target/release/examples/plugin_host target/release/examples/librepe_plugin.so
```

That is what keeps the checks in it protocol-level. Behavior that is this
crate's rather than the protocol's — the response echoing the request query,
which Glaze's registry does not do — is pinned in `tests/plugin_host.rs`, where
the plugin under test is known.
