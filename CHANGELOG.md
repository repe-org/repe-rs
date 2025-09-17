# Changelog

## [0.1.2] - 2025-09-17
- Stream sync and async REPE message I/O directly into final buffers to avoid extra allocations and copies.
- Reuse persistent buffered readers/writers in the TCP client to eliminate per-request socket clones.
