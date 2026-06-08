# Embedder Friction: HTTP Co-Hosting Ergonomics and Embedder-Keyed Peer Lookup

## Status

Shipped in 3.2.0. Both friction points were addressed: co-hosting via `is_websocket_upgrade` + `examples/websocket_cohosting.rs` + `HandshakeContext` / `on_peer_connect_with_handshake`; embedder-keyed lookup via `PeerRegistry::alias` / `get_by` / `key_for` / `aliases_for`. Retained as the design record behind those additions.

## Summary

Two friction points surfaced while building a server-push application on top of the shipped `WebSocketServer` + `PeerRegistry` surface. Neither is a blocker; both have working manual workarounds today. This document records them as requests so the core crate can decide whether to smooth them over.

1. **Co-hosting plain-HTTP routes on the WebSocket port is possible but all-manual.** An embedder that needs a few sibling HTTP routes (a liveness probe, a couple of small JSON `GET`/`POST` endpoints) alongside a REPE WebSocket endpoint must own the TCP accept loop, classify each connection itself, and bring its own HTTP/1.1 stack. There is no turnkey recipe or helper for the common peek-then-fork shape.

2. **`PeerRegistry` is keyed only by `PeerId(u64)`.** An embedder whose canonical client identity is something other than a `u64` (a UUID/token from the handshake, a domain id) must maintain a parallel `HashMap<TheirKey, PeerId>` and keep it in sync on connect/disconnect, re-implementing exactly the book-keeping `PeerRegistry` removes for the broadcast case.

Both requests are framed to stay additive and non-breaking, consistent with the crate's stated principle of preferring additive APIs and deferring higher-level surfaces until a second distinct consumer exists.

---

## Friction 1: co-hosting plain-HTTP routes on the WebSocket port

### Scenario

A service exposes a REPE-over-WebSocket data feed and also wants a handful of plain HTTP routes on the **same** TCP port:

- `GET /healthz` for an orchestration liveness probe.
- A small number of `GET`/`POST` JSON endpoints for out-of-band control (e.g. a config read/apply pair, or a "resolve this path" request) that are awkward to model as REPE calls because non-REPE tooling (a browser, `curl`, a probe) issues them.

### What already exists

This is not impossible today. `WebSocketServer::into_shared()` yields a `SharedWebSocketServer`, and `WebSocketServer::accept` plus `SharedWebSocketServer::serve_connection[_with_cancel]` let an embedder own its own accept loop and serve individual upgraded connections. The doc comment on `into_shared` (`src/websocket_server.rs:614`) describes exactly the intended use:

> Use with `accept` and `SharedWebSocketServer::serve_connection` to serve connections the embedder accepts itself — e.g. peek the upgrade header on each accepted stream, route WebSocket upgrades to REPE and send everything else to an HTTP handler, all on one TCP port.

There is also a `## One-Port Co-Hosting` section in `docs/websocket.md` (with a peek-then-fork code block and a draining subsection), so the escape hatch is deliberate, present, and already partially documented.

### The friction

The escape hatch removes "impossible" but leaves real boilerplate, and it forces a dependency decision. To co-host even a single `/healthz` route, the embedder must:

1. Own the `TcpListener` accept loop instead of calling `serve` / `serve_listener`.
2. Peek the first bytes of each accepted stream and classify it as a WebSocket upgrade vs. a plain HTTP request (the WS-vs-HTTP fork). repe exposes no helper for this classification, so each embedder reinvents the peek-and-sniff.
3. Bring a full HTTP/1.1 implementation, or hand-roll request-line/header parsing and response writing, purely to answer trivial routes.

The practical outcome: a project whose only network dependency would otherwise be repe ends up pulling in a second HTTP framework solely to serve a health check beside the WebSocket endpoint, or hand-writing HTTP/1.1 against a raw stream. The existing `docs/websocket.md` co-hosting section shows the *shape*, but its example stubs out exactly the two hardest parts as hand-waved placeholders: `looks_like_websocket_upgrade(&stream)` (the WS-vs-HTTP classifier, `docs/websocket.md:316`) and `serve_http(stream)` (the plain-HTTP side). Neither has a worked, compiling implementation in the crate, so the two pieces an embedder cannot easily write itself are precisely the two the documentation leaves as an exercise.

### Requests (graduated, additive)

**R1 (recommended first; cheap, no API change):** a compiling co-hosting example that fills in the two stubs the current docs leave open.

- An `examples/` program that owns the accept loop, classifies each stream, forks WS upgrades to `serve_connection` and plain HTTP to a tiny embedder-supplied handler, all on one port. The `docs/websocket.md` co-hosting section already shows the shape; the gap is a real `looks_like_websocket_upgrade` and a minimal `serve_http`, not another prose section. Link the example from that section.
- Optionally a small classifier helper, e.g. `is_websocket_upgrade(&stream) -> io::Result<bool>`, so embedders do not each reinvent the sniff. One hard constraint shapes its API: `WebSocketServer::accept(stream, path)` consumes the original `TcpStream` and replays the handshake itself (`src/websocket_server.rs:643`), so the helper must be **non-destructive** — a `TcpStream::peek` that leaves the request bytes intact for the subsequent `accept`. A helper that parsed the request line/headers would consume them and break `accept`; handing back parsed headers would instead require changing `accept` to take a buffered stream. So the helper stays a small peek-and-sniff, which keeps repe out of the business of *serving* HTTP while removing the trickiest part of the fork.

**R2 (larger; defer until a second consumer appears):** an optional minimal plain-HTTP route table co-hosted on the same listener for simple request to response routes (a health endpoint plus small JSON `GET`/`POST` handlers), gated behind a feature.

- This is closer to "repe grows a tiny HTTP surface," which is a scope expansion. It should follow the crate's own deferral discipline: build it only once a second, distinct embedder needs co-hosted HTTP, so the surface is designed against more than one shape rather than over-fit to the first.
- R1 fully unblocks the common case without taking on that scope, so R2 is genuinely optional.

---

## Friction 2: `PeerRegistry` is keyed only by `PeerId(u64)`

### Scenario

An embedder's canonical client identity is not a `u64`. For example each connection is identified by a UUID/token string assigned from the WebSocket handshake (a query parameter or auth header), and the rest of the embedder's code addresses clients by that string: targeted pushes to "client X," per-client metrics, disconnect handling.

### What exists

`PeerRegistry` stores `Arc<Mutex<HashMap<PeerId, PeerHandle>>>` and is keyed exclusively by `PeerId(pub u64)` (`src/peer.rs`):

- `get(PeerId) -> Option<PeerHandle>` and `remove(PeerId)` are `PeerId`-only.
- `broadcast_notify_*` returns `HashMap<PeerId, Result<(), PeerSendError>>`.
- `PeerHandle { peer_id, sink }` carries no embedder-supplied identity.
- `WebSocketServer::with_peer_registry` auto-inserts each accepted peer keyed by the server-minted `PeerId`; this auto-insert is the registry's main ergonomic draw.

### The friction

For a **targeted** push by the embedder's own identity, the embedder must keep a parallel `HashMap<TheirKey, PeerId>` (or `HashMap<TheirKey, PeerHandle>`) and sync it on connect/disconnect, which re-implements precisely the live-peer book-keeping that `PeerRegistry` exists to remove. The broadcast result map keyed by `PeerId` is also opaque to the embedder: it cannot tell which of *its own* clients hit `PeerSendError::Full` / `Disconnected` without consulting that same side map.

There is a related sub-friction at the natural insertion point. `on_peer_connect` takes `Fn(PeerHandle)` and receives only the `PeerHandle`; it gets no handshake context (request path, query, headers). So an embedder whose key is derived from the upgrade request cannot compute its key at the connect hook and must capture the mapping through a separate channel, adding to the parallel-map bookkeeping.

### Requests (additive options; Option B recommended)

**Option A: generic `PeerRegistry<K = PeerId>`.** Key the registry by an embedder-chosen type with `PeerId` as the default.

- Tradeoffs: this changes `with_peer_registry`'s signature and the broadcast return type to `HashMap<K, _>` (a semver-sensitive ripple even with a default type param), and it conflicts with the server-driven auto-insert path. The server only knows the `PeerId` it mints; it cannot supply a custom `K`, so a generic-keyed registry could not be auto-populated by `with_peer_registry` without the embedder also providing a key-derivation step. That undercuts the registry's main convenience.
- Not recommended as the primary path for those reasons.

**Option B (recommended): an optional alias index; `PeerId` stays canonical.** Keep `PeerId` as the primary key so `with_peer_registry` auto-insert is unchanged, and add an embedder-supplied secondary lookup:

- `alias(peer_id, key)` to associate an embedder key with an already-inserted peer, `get_by(&key) -> Option<PeerHandle>`, and alias cleanup folded into `remove` so a disconnect drops both the primary entry and any aliases. Cleanup-on-`remove` implies the registry also keeps a reverse `PeerId -> {alias}` index, so a close drops aliases without scanning the whole alias map; since `with_peer_registry` already routes disconnect through `remove` (`src/websocket_server.rs:351`), folding the purge into `remove` is the right seam.
- The embedder adds the alias once it knows its key, the primary auto-insert path is untouched, and the change is fully additive. A generic alias key (`get_by<Q>`) keeps it flexible without genericizing the whole registry.
- **Targeted push, not broadcast interpretation.** Forward-only aliasing solves a targeted push by key (`get_by(&key).send_notify(...)`), but it does *not* let the embedder read a `broadcast_*` result map by its own identity: those return `HashMap<PeerId, _>` (`src/peer.rs:461`), so answering "which of *my* clients hit `Full`?" still needs `PeerId -> key`. Close this by also exposing the reverse index B already maintains for cleanup (e.g. `key_for(peer_id) -> Option<&K>`), or by adding a keyed broadcast-result variant. This is the one place Option C (identity carried on `PeerHandle`) is strictly better: the identity then rides along in `peers()` and in each result handle, where B needs the extra reverse accessor to match it.

**Option C: opaque embedder tag on `PeerHandle`.** Let `PeerHandle` optionally carry an embedder value (e.g. `Arc<dyn Any + Send + Sync>` or a small generic tag) so the embedder's identity travels with the peer and is returned by `peers()` / `get()`. Heavier than B and pushes a type parameter or `Any` downcast onto `PeerHandle`; mentioned for completeness, B is cleaner.

**Required companion when the key lives in the handshake:** giving `on_peer_connect` access to the handshake request context (path / query / headers), or a sibling hook that does, so the embedder can compute its key/alias at accept time. For the scenario above — a key carried as a query parameter or auth header — this is not merely ergonomic. `on_peer_connect` is `Fn(PeerHandle)` and sees no request context (`src/websocket_server.rs:365`), so without it Option B only *relocates* the parallel bookkeeping (the embedder still needs a side channel to learn key -> `PeerId` before it can call `alias`) rather than removing it. The coupling is conditional on where the key lives: if it instead arrives in a request *body*, the handler already has both the key and the peer via `CallContext::peer` (`src/peer.rs:276`), and the connect hook is unnecessary. Either way this is a small, independently useful additive hook improvement.

---

## Alignment with the crate's design principles

- **R1** and **Option B** are additive and non-breaking, and neither grows repe toward being a web framework or a general identity store. They remove embedder boilerplate around surfaces repe already owns (the accept/fork path; the live-peer set).
- **R2** and **Option A** are larger and semver-sensitive; both are flagged as deferrable, consistent with the established discipline of holding higher-level surfaces until a second distinct consumer exists.

## What "resolved" looks like

- **Friction 1:** an embedder can co-host `GET /healthz` plus a couple of small JSON routes beside a REPE WebSocket endpoint on one TCP port without adding a second HTTP framework, by following a documented repe recipe (R1) or via an optional built-in route table (R2).
- **Friction 2:** an embedder can issue a targeted push addressed by its own client identity without maintaining a parallel `PeerId` map kept in sync by hand (Option B alone), and — once the reverse `PeerId -> key` accessor or a keyed broadcast-result variant is added — can also interpret broadcast results by that identity.
