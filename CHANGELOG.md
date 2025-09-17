# Changelog

## [0.1.3] - 2025-09-17
- Added zero-copy query routing so servers reuse borrowed UTF-8 query slices, cutting per-request allocations and tightening error handling for invalid query encodings.

## [0.1.2] - 2025-09-17
- Stream sync and async REPE message I/O directly into final buffers to avoid extra allocations and copies.
- Reuse persistent buffered readers/writers in the TCP client to eliminate per-request socket clones.
