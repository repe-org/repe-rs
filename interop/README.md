# REPE C++ interop fixtures

This directory pins wire compatibility between `repe` (this crate) and the
canonical C++ REPE implementation in [Glaze](https://github.com/stephenberry/glaze).
Both implement REPE **version 1** (the 48-byte header). See
[`docs/interop.md`](../docs/interop.md) for the compatibility guarantee and the
known v1-vs-v2 spec divergence.

## Layout

- `cpp/generate_fixtures.cpp` — a small program that links Glaze and emits
  authentic REPE frames (it is the *only* source of the bytes; nothing here is
  hand-authored).
- `cpp/CMakeLists.txt` — builds the generator; fetches Glaze at a pinned tag by
  default.
- `fixtures/*.repe` — committed raw REPE frames produced by the generator.
- `fixtures/manifest.json` — committed; describes the expected decode of each
  frame. Consumed by `tests/interop.rs`.

The C++ generator and the fixtures are **dev-only**: `interop/` is excluded from
the published crate (it is not in `Cargo.toml`'s `include` list). The Rust tests
in `tests/interop.rs` read `fixtures/` at runtime.

## Regenerating the fixtures

Requires CMake and a C++23 compiler. By default Glaze is fetched at the pinned
tag (`v7.7.1`) so the committed bytes and CI stay in lockstep:

```sh
cmake -S interop/cpp -B interop/cpp/build
cmake --build interop/cpp/build
interop/cpp/build/generate_fixtures interop/fixtures v7.7.1
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
