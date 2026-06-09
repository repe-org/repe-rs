# Typed Numeric Body Fast Path — remaining work

## Status

The core fast path shipped in **repe 3.4.0**: `MessageBuilder::body_typed_slice` / `body_complex_slice` (bulk encode), `Message::decode_typed_slice` / `decode_complex_slice` (bulk decode), `io::write_message_typed_slice` (zero-buffer streaming framing), `RepeError::UnexpectedBodyFormat`, and the re-exported `beve::{BeveTypedSlice, Complex}`. It is built on beve 2.0's bulk slice helpers (`to_vec_typed_slice` / `to_writer_typed_slice` / `typed_slice_size` / `read_typed_slice` / `to_vec_complex_slice` / `read_complex_slice`). See `docs/numeric-bodies.md` and `examples/typed_numeric_body.rs`.

This file now tracks only what was deliberately left out, with the context needed to pick each one up.

## Future work

### Complex streaming writer — shipped in 3.5.0

`write_message_typed_slice` was zero-buffer for numeric slices but had no streaming complex equivalent, because beve had only `to_vec_complex_slice` (allocates the whole body) and no streaming writer. Closed: beve 2.1.0 added `to_writer_complex_slice<W, T>` + `complex_slice_size` (the streaming counterpart of `to_vec_complex_slice`, mirroring `to_writer_typed_slice` / `typed_slice_size`), and repe 3.5.0 added `write_message_complex_slice` in `src/io.rs`, sized by `complex_slice_size` and written by `to_writer_complex_slice` — symmetric with the numeric path. Retained here as the record of the gap.

### `with_typed_slice` handler route

The bulk encode/decode currently lives on `MessageBuilder` / `Message`; a server handler still has to call those by hand. A `Router::with_typed_slice::<T>(path, f)` that decodes a whole-body numeric/complex request into `Vec<T>` and bulk-encodes the `Vec<R>` response would close the loop for routes whose whole body is an array.

- **Deferred until a second consumer exists** (matches the crate's "hold higher-level surfaces until a second distinct consumer" discipline). The free-method shape on `Message` is the proving ground; promote to a route trait only once a real handler wants it.

### Bulk matrix body path

`beve::Matrix` / `MatrixOwned` round-trip through serde today via `body_beve` / `with_typed`. A matrix is a header plus a typed-slice payload, so it can reuse the same bulk primitives, but exposing a `body_matrix` / `decode_matrix` surface cleanly is its own design pass. Out of scope until the slice path has a real matrix consumer.

### Minor / opportunistic

- **`into_wire_bytes` headroom — shipped in 3.5.1.** `body_typed_slice` / `body_complex_slice` previously called `to_vec_*_slice`, which reserves an exact-fit body `Vec` with no room for the wire prefix, so the outbound `into_wire_bytes` fast path (`src/message.rs`) couldn't reuse it. Closed: both builders now size the body buffer with `Vec::with_capacity(typed_slice_size + HEADER_SIZE + query.len())` and write into it via `to_writer_*_slice`, so a `body_typed_slice` message built after its query frames with zero further allocation through `into_wire_bytes` (verified by pointer-identity tests). Still single-allocation encode; wire bytes unchanged. Only matters for the build-a-Message-then-frame pattern, not the already-zero-buffer streaming path.
- **Big-endian repe-layer test.** The fast path relies on beve's BE fallback (bulk copy is little-endian-only; BE converts per element). repe has no BE test of its own; a cfg-gated or cross-compiled round-trip would pin it at the repe layer instead of trusting beve's BE coverage transitively.
- **beve `read_bool_slice` / `read_str_slice`.** beve's bulk read side still only covers numeric and complex; bool (bit-packed) and string (length-prefixed) arrays have writers but no bulk readers. Only worth adding a repe bool/string body fast path if such a use case appears — numeric/complex is the real demand.
