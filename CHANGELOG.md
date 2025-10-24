# Changelog

## [Unreleased]
- Router shared-struct registration now accepts any `Lockable` lock, including `tokio::sync`
  mutexes/RwLocks out of the box and `parking_lot` locks when the optional feature is enabled.

## [0.2.0] - 2025-09-18
- Added full BEVE body support (builder helpers, response serialization, and message decoding) backed by the official `beve` crate.
- Server routers and typed handlers now accept BEVE payloads, mirroring existing JSON ergonomics.
- Documented BEVE usage and updated the spec reference URL; expanded tests to cover complex BEVE round-trips.

## [0.1.3] - 2025-09-17
- Added zero-copy query routing so servers reuse borrowed UTF-8 query slices, cutting per-request allocations and tightening error handling for invalid query encodings.

## [0.1.2] - 2025-09-17
- Stream sync and async REPE message I/O directly into final buffers to avoid extra allocations and copies.
- Reuse persistent buffered readers/writers in the TCP client to eliminate per-request socket clones.
