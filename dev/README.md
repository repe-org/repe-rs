# dev/ — design records and work trackers

Internal design notes for repe-rs: the rationale behind shipped features and the scope of work still planned. These are **not** API documentation (see `docs/` and the rustdoc for that) — they are the record of *why* a feature is shaped the way it is, kept so a future maintainer can recover the reasoning.

Each doc carries a `## Status` line. Three kinds live here:

**Shipped design records** — the finalized design for a feature that has landed, retained as history:

- [`push-capable-websocket-server.md`](push-capable-websocket-server.md) — server-initiated WebSocket push (PeerRegistry, outbound channel, broadcast helpers, lifecycle hooks). Shipped in 2.5.0.
- [`http-cohosting-and-keyed-peer-registry.md`](http-cohosting-and-keyed-peer-registry.md) — one-port HTTP/WebSocket co-hosting and embedder-keyed peer lookup. Shipped in 3.2.0.

**Live trackers** — docs with open work still to do:

- [`feature-expansion-plan.md`](feature-expansion-plan.md) — post-Registry roadmap. Generic call/notify, fleet, and UniUDP shipped; the Glaze/C++ interop test suite is still open.
- [`typed-numeric-body-fast-path.md`](typed-numeric-body-fast-path.md) — numeric/complex body fast path. Core shipped in 3.4.0; remaining items (complex streaming writer, `with_typed_slice` route, matrix path) are tracked here.
- [`unknown-field-tolerance.md`](unknown-field-tolerance.md) — make "ignore unknown request-body object keys" a documented protocol stance and an explicit per-server/per-endpoint opt-in, so a newer client adding an optional field does not break an older server. The protocol stance, the documented guarantee, and its tests shipped in #37; the strictness opt-in (`UnknownFieldPolicy`, the derive attribute, BEVE parity) is still open.

**Deferred decisions** — a design that was investigated and deliberately *not* built, kept so the question is not reopened from scratch:

- [`svs-streaming-producer-integrity.md`](svs-streaming-producer-integrity.md) — single-pass end-to-end integrity for a lazy SVS producer. `with_writer_stream` (5.0.0) already covers app-framed `payload || digest` streams tagged RawBinary, so the residual gap is narrow: single-pass integrity while keeping a standard BEVE/JSON/UTF-8 `format` tag, where appending digest bytes would break the tag. Deferred by decision, since closing it is a wire change and SVS v1 is frozen for interop with content integrity an explicit non-goal. Records the trigger that would justify revisiting and the shape to use if it fires (a backward-compatible companion route under `/_svs`, not a v2).

Local tooling state (`dev/.claude/`) is gitignored.
