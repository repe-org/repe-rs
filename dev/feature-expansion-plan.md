# REPE-RS Feature Expansion Plan (Post-Registry)

## Status

Delivered. All four tracked items have shipped:

- **Shipped:** generic call/notify APIs (`call_with_formats` / `notify_with_formats`), Fleet orchestration (`fleet` / `async_fleet`), and the UniUDP transport stack (`udp_client` / `uniudp_fleet`).
- **Shipped:** the Glaze/C++ interoperability test suite (item 1). A C++ fixture generator (`interop/cpp/`) links the canonical Glaze REPE implementation and emits authentic REPE v1 frames; those bytes and a manifest are committed under `interop/fixtures/`; `tests/interop.rs` asserts parse parity, body decode, byte-identity round-trip, and (for protocol-defined layouts) from-scratch encoder parity; and a gated `interop` CI workflow rebuilds the generator from the pinned Glaze tag, regenerates, and fails on drift. See `docs/interop.md`. The rest of this doc is retained as the record of how the work was staged.

Note (cross-library): both repe-rs and the released Glaze implement REPE **v1** (48-byte header), so the suite pins their mutual compatibility. The REPE spec has since defined **v2** (32-byte header); migrating both implementations is separate, coordinated future work.

## Purpose
Registry parity has been merged. This document tracks the remaining expansion work:
1. Glaze/C++ interoperability test suite
2. Generic call/notify APIs (raw/utf8/custom formats)
3. Fleet orchestration
4. UniUDP transport stack (if roadmap requires one-way/lossy links)

The goals are cross-language protocol compatibility, stable API evolution, and staged delivery with clear validation at each phase.

## Guiding Principles
- Keep protocol correctness ahead of convenience APIs.
- Ship in small, testable milestones with clear exit criteria.
- Prefer additive APIs and avoid breaking existing `Client`, `AsyncClient`, and `Router` behavior.
- Gate large optional systems (`fleet`, `uniudp`) behind feature flags when practical.

## Delivery Order
1. Interop suite against Glaze/C++
2. Generic call/notify APIs
3. Fleet orchestration
4. UniUDP transport

Rationale:
- Interop tests lock protocol compatibility before broader API growth.
- Generic call/notify provides primitives needed by fleet and transport extensions.
- Fleet and UniUDP are larger systems and should build on verified wire behavior.

---

## Phase 1: Glaze/C++ Interoperability Test Suite

### Objectives
- Verify cross-language protocol compatibility continuously.
- Catch encoding/error-code/format mismatches early.

### Scope
- Add C++ interop fixture server and automated tests (JSON first, then BEVE/UTF-8/raw where applicable).
- Add optional CI job (gated or matrix-based) to run interop tests.

### Implementation Plan
1. Add `interop/` assets (C++ server source/build scripts) or reference a pinned fixture.
2. Add Rust integration tests that:
  - Launch the fixture process.
  - Run request/notify scenarios.
  - Assert header fields, body decoding, and error propagation.
3. Add CI workflow coverage for interop tests (allow opt-out in minimal environments).

### Test Matrix
- Request/response:
  - JSON success paths
  - Error responses and error-code mapping
  - Method-not-found behavior
  - Notify no-response behavior
- Formats:
  - JSON baseline
  - BEVE where supported
  - UTF-8 and raw handling through generic APIs once Phase 2 lands
- Header validation:
  - version mismatch
  - malformed lengths/spec handling

### Exit Criteria
- Interop tests pass locally and in CI.
- Compatibility expectations are documented in README/CHANGELOG.

### Key Risks
- Environment/toolchain friction for C++ builds in CI.
- Fixture version drift causing flaky interop baselines.

---

## Phase 2: Generic Call/Notify APIs (Raw/UTF-8/Custom)

### Objectives
- Provide low-level client/server primitives that are format-agnostic.
- Keep existing JSON and typed helpers as convenience wrappers.

### Proposed API Direction
- Client (`Client` and `AsyncClient`):
  - Generic call taking query bytes/string, `query_format`, body bytes/value, and `body_format`.
  - Generic notify with same flexibility.
  - Optional raw response access (`Message`) without forced JSON decode.
- Server:
  - Handler path that can receive full message/body bytes for custom body formats.
  - Preserve existing typed/JSON handler ergonomics.

### Implementation Plan
1. Introduce generalized request builders and response decoders.
2. Add enum/newtype strategy for custom format codes (`>= 4096`).
3. Implement and stabilize `call_raw`/`notify_raw` style APIs (sync + async).
4. Refactor existing JSON/typed methods to use generic core.
5. Add full docs and migration examples.

### Testing
- Unit tests:
  - Custom format code round-trips.
  - Raw body passthrough integrity.
  - UTF-8 and raw decode behavior.
- Integration tests:
  - Generic API with existing server.
  - Existing `call_json`/typed methods unchanged.
  - Batch + timeout behavior with generic requests where supported.

### Exit Criteria
- Public generic call/notify APIs are available in sync and async clients.
- Existing JSON/typed public APIs remain backward-compatible.

### Key Risks
- API complexity creep if too many entry points are added at once.
- Custom-format handling must not weaken validation guarantees.

---

## Phase 3: Fleet Orchestration Layer

### Objectives
- Add multi-node orchestration over TCP REPE:
  - node config
  - connect/disconnect/reconnect
  - call, broadcast, map-reduce
  - retries and health checks
  - optional tag filtering

### Proposed API Direction
- `repe::fleet` module with:
  - `NodeConfig`
  - `Fleet`
  - result types (`RemoteResult` equivalent)
  - helper predicates (`succeeded`, `failed`) or Rust-idiomatic variants

### Implementation Plan
1. Implement core sync fleet using `Client`.
2. Add parallel call/broadcast execution with bounded worker/task strategy.
3. Add retry policy and per-node timeout support.
4. Add health-check support via configurable endpoint.
5. Optionally add async fleet after sync API stabilizes.

### Testing
- Unit tests:
  - config validation
  - node add/remove and duplicate-name handling
  - retry policy behavior
- Integration tests:
  - broadcast and map-reduce over multiple local servers
  - partial failure behavior
  - health-check correctness

### Exit Criteria
- Fleet supports production-basic orchestration with predictable error reporting.
- Clear docs for parallel behavior and retry semantics.

### Key Risks
- Concurrency model and cancellation behavior can get complex quickly.
- Need careful defaults to avoid surprising retry storms.

---

## Phase 4: UniUDP Transport Stack (Roadmap-Conditional)

### Gate
Proceed only if product roadmap includes one-way/lossy-link requirements.

### Objectives
- Add REPE over unidirectional UDP with chunking and redundancy.
- Add optional FEC support and message assembly/dedup behavior.
- Provide clear boundaries: fire-and-forget semantics vs request/response expectations.

### Proposed Architecture
- New module or crate (`repe-uniudp`) with feature flag integration.
- Core pieces:
  - packet format
  - sender chunking/redundancy
  - receiver assembly/timeouts
  - deduplication window
  - optional parity/FEC recovery
- API wrappers:
  - `UniUdpClient` and `UniUdpServer`
  - optional `UniUdpFleet` after base transport stabilizes

### Implementation Plan
1. Deliver minimal MVP:
  - chunking
  - redundancy
  - receiver assembly
  - inactivity/overall timeout behavior
2. Add FEC parity mode.
3. Add metrics/reporting surfaces.
4. Add higher-level REPE wrappers and examples.

### Testing
- Property/integration tests:
  - packet encode/decode
  - chunk loss simulations
  - out-of-order delivery
  - duplicate packet handling
  - timeout completion reasons
- Optional stress/soak tests for high-throughput scenarios.

### Exit Criteria
- Documented reliability profile under controlled packet-loss scenarios.
- Clear production caveats and recommended configuration ranges.
