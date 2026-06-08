# dev/ — design records and work trackers

Internal design notes for repe-rs: the rationale behind shipped features and the scope of work still planned. These are **not** API documentation (see `docs/` and the rustdoc for that) — they are the record of *why* a feature is shaped the way it is, kept so a future maintainer can recover the reasoning.

Each doc carries a `## Status` line. Two kinds live here:

**Shipped design records** — the finalized design for a feature that has landed, retained as history:

- [`push-capable-websocket-server.md`](push-capable-websocket-server.md) — server-initiated WebSocket push (PeerRegistry, outbound channel, broadcast helpers, lifecycle hooks). Shipped in 2.5.0.
- [`http-cohosting-and-keyed-peer-registry.md`](http-cohosting-and-keyed-peer-registry.md) — one-port HTTP/WebSocket co-hosting and embedder-keyed peer lookup. Shipped in 3.2.0.

**Live trackers** — docs with open work still to do:

- [`feature-expansion-plan.md`](feature-expansion-plan.md) — post-Registry roadmap. Generic call/notify, fleet, and UniUDP shipped; the Glaze/C++ interop test suite is still open.
- [`typed-numeric-body-fast-path.md`](typed-numeric-body-fast-path.md) — numeric/complex body fast path. Core shipped in 3.4.0; remaining items (complex streaming writer, `with_typed_slice` route, matrix path) are tracked here.

Local tooling state (`dev/.claude/`) is gitignored.
