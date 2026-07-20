# Unknown-Field Tolerance for Request Bodies

## Status

**Partly shipped (#37); the strictness opt-in is still open.** Implementation Plan
step 1 landed: the protocol stance is stated in `docs/protocol.md`, the tolerant
default is documented as a guaranteed property in `docs/server.md`, and the
guarantee is pinned end to end for JSON and BEVE in `tests/unknown_fields.rs`.
The cross-implementation case went further than step 1 planned, with Glaze-generated
fixtures decoding into an older Rust struct (`interop/fixtures/*_request_unknown_key.repe`,
`tests/interop.rs`); the reverse direction (a Glaze server tolerating an extra key
from repe, which depends on Glaze's `error_on_unknown_keys` default) remains
follow-up. No `src/` change was needed, since repe-rs was already tolerant.

**Still open: steps 2 through 5** — the `UnknownFieldPolicy` type, the per-server
default and per-endpoint override, the `Reject` decode path, the
`#[repe(deny_unknown)]` derive attribute, and BEVE parity. Everything below from
"Proposed Design" onward describes that remaining work; the parts of the Test
Matrix and Exit Criteria covering the tolerant default and the interop case are
already satisfied.

Original motivation, retained because it is what the design answers to: a
cross-implementation version-skew break, where a client that added an optional
field to a request body talked to a server build that predated the field, and the
server's serializer rejected the whole request on the unknown key. The Rust
implementation never had this failure mode (it is tolerant by accident of serde's
defaults), but the protocol was silent on the question and at least one conforming
implementation defaults to strict rejection, so the behavior was neither guaranteed
nor consistent across the ecosystem. Step 1 closed the "is the leniency
intentional?" gap; making strictness a deliberate, explicit choice is what remains.

## Summary

Make "tolerate unknown object fields in a request body" a deliberate, documented
property of REPE rather than an unstated side effect of whichever serializer an
implementation happens to use. Concretely, this request has two parts:

1. **Protocol stance.** State in the spec that a conforming server SHOULD accept
   unknown object keys in a request body (ignore-and-continue) by default, so that a
   newer client can add an optional field without breaking an older server. Strict
   rejection of unknown keys is a legitimate but *explicit, opt-in* choice, not the
   default. This is distinct from unknown body *format codes*, which remain a hard
   error (see Current Behavior).

2. **repe-rs surface.** Document repe-rs's current tolerant behavior as a guaranteed
   forward-compatibility property, and add an explicit, additive opt-in for servers
   that *want* schema enforcement: a per-server default plus a per-endpoint override,
   and a derive attribute for `RepeStruct` handlers. Strictness becomes a deliberate
   decision a server makes, not an accident of the codec.

Everything here is additive and opt-in. Servers that do nothing keep today's tolerant
behavior unchanged.

## Motivation

REPE links are long-lived and frequently span two versions at once. A client picks up
a new field before the server it talks to has been redeployed, or one peer in a fleet
runs an older image than the rest, or a server is an embedded device updated on a
slower cadence than its clients. In all of these, the safe failure mode for an
*unknown optional field* is to ignore it: the request still carries everything the
older server needs, plus one key it does not recognize.

The unsafe failure mode is to reject the entire request because of that one key. When a
server does this, adding any optional field to a request body becomes a synchronized,
lock-step deployment: every server must be upgraded before any client may send the new
field. That is exactly the coupling REPE's pluggable, self-describing bodies are
supposed to avoid, and it turns a backward-compatible schema addition into a breaking
change.

The behavior is currently left entirely to the serializer:

- A Rust REPE server built on this crate is **tolerant**: serde ignores unknown object
  fields, so the request decodes and the extra key is dropped.
- A C++ REPE server built on Glaze is **strict by default**: Glaze's
  `error_on_unknown_keys` rejects the request on the first undeclared key.

Two conforming implementations, opposite behavior, and nothing in the protocol says
which is correct. A client cannot know whether sending a forward-looking field is safe
without knowing the server's exact build and serializer configuration. That is the gap.

## Current Behavior

repe-rs delegates request-body decoding directly to serde / BEVE with no configuration,
so unknown object fields are silently ignored on every decode path:

- Typed handlers: `decode_typed_param` (`src/server.rs:413-429`) and
  `decode_typed_param_view` (`src/server.rs:434-450`) call `serde_json::from_slice`
  with default options.
- Registry bodies: `Registry::decode_body` (`src/registry.rs:314-333`) decodes JSON via
  `serde_json::from_slice::<Value>`.
- `RepeStruct` handlers: `RegisteredStruct::handle` (`src/server.rs:1599-1618`) and the
  derive macro (`repe-derive/src/lib.rs:52-56`, `386-389`, `418-422`, `474-477`) decode
  through `serde_json::from_value`.
- BEVE bodies go through `beve::from_slice` on the same paths.

The behavior is uniform across the JSON and BEVE body formats, and there is no
`#[serde(deny_unknown_fields)]` anywhere in the crate. There is also no options struct
to change it: `Router` (`src/server.rs:967-973`) holds only its handler maps and
middleware, `AsyncServer` (`src/async_server.rs:12-16`) holds only `read_timeout` and
`write_timeout`, and `Registry` (`src/registry.rs:137-139`) holds only its state lock.
So today's leniency is real and reliable, but it is undocumented and unconfigurable: a
server cannot opt into strict schema enforcement, and a reader of the code cannot tell
that the leniency is intentional rather than incidental.

This is separate from unknown body *format codes*. Body formats are an enum
(`RawBinary`, `Beve`, `Json`, `Utf8`; `src/constants.rs:115-120`) with codes `0..=4095`
reserved for REPE (`src/constants.rs:85,111`), and an unrecognized format code is
correctly a hard error (`InvalidBody`; `docs/registry.md:58`). That should not change.
The request here is only about unknown *object keys within* a structured body.

The protocol docs say nothing either way. REPE versions the wire envelope (the 48-byte
v1 header; `docs/interop.md:10-11`) and is deliberately loose about body encoding
details (JSON parity is checked semantically, not byte-for-byte;
`docs/interop.md:45-50`), but there is no statement about schema evolution, optional
fields, or unknown-key handling for bodies.

## Proposed Design

### 1. Protocol recommendation

Add a short "Schema evolution" note to the protocol documentation stating:

- A request body's structured object MAY gain new keys over time. A conforming server
  SHOULD ignore keys it does not recognize and decode the rest, so that older servers
  remain interoperable with newer clients.
- Strict rejection of unknown keys is permitted but is an explicit server choice and
  MUST NOT be the default. A server that rejects unknown keys should surface a clear,
  specific error (which key, at which path).
- This applies to keys within a structured body. Unknown body *format codes* remain a
  hard error and are out of scope.

This is the part that actually fixes the cross-implementation friction: it gives every
implementation, including the Glaze-based C++ server, a single recommended default to
align on.

### 2. repe-rs configuration surface

Document the tolerant default as a guarantee, and add an explicit opt-in for the
opposite. The natural shape, mirroring the additive/opt-in pattern used by the push
surface:

- **Per-server default.** A builder on `Router` (or `RouterBuilder`) sets the
  unknown-field policy for all handlers, defaulting to `Tolerate`:

  ```rust
  let router = Router::builder()
      .unknown_fields(UnknownFieldPolicy::Reject) // default: Tolerate
      .register(/* ... */)
      .build();
  ```

- **Per-endpoint override.** An individual handler can override the server default in
  either direction, so a server can be tolerant everywhere except on the one endpoint
  where it wants to catch client typos, or strict everywhere except on a
  forward-compatible endpoint.

- **Derive attribute for `RepeStruct`.** A `#[repe(deny_unknown)]` attribute at the
  struct or field level, alongside the existing `#[repe(skip)]` / `#[repe(nested)]` /
  `#[repe(readonly)]` / `#[repe(rename)]` attributes (`repe-derive/src/lib.rs:105-159`),
  so struct-registered handlers can express the policy declaratively.

Mechanically, `Tolerate` is the current code path (serde defaults). `Reject` routes the
JSON decode through a deserializer configured to error on unknown fields and maps the
failure to a precise REPE error naming the offending key and JSON Pointer path. The
BEVE path gets the equivalent treatment where the codec supports it (see Open
Questions).

## Implementation Plan

1. **Document the current guarantee.** ✅ **Shipped (#37).** Add the "Schema evolution"
   note to the protocol docs and a sentence to the server docs stating that unknown
   request-body object keys are ignored by default. No code change; this alone closes
   the "is the leniency intentional?" gap and gives the ecosystem a stance to align on.

2. **Policy type + per-server default.** Introduce `UnknownFieldPolicy { Tolerate,
   Reject }` and thread a server-level default through `Router`. `Tolerate` preserves
   today's behavior bit-for-bit.

3. **Reject path for JSON.** Implement `Reject` for the typed and registry JSON decode
   paths with a precise error (key + pointer). Add the per-endpoint override.

4. **Derive attribute.** Add `#[repe(deny_unknown)]` to repe-derive and wire it to the
   per-endpoint policy for `RepeStruct` handlers.

5. **BEVE parity.** Extend `Reject` to BEVE bodies, or document explicitly that
   `Reject` is JSON-only if the BEVE codec cannot express it.

## Cross-Implementation Note

The motivating break was a strict C++/Glaze server rejecting a request from a newer
client. The protocol recommendation in part 1 is what resolves that class of bug at the
ecosystem level: it gives the C++ implementation a documented reason to default its
REPE server to tolerant decoding (or to make strict an explicit opt-in), so that a
client adding an optional field degrades gracefully against an older server instead of
hard-failing. The interop suite tracked in `feature-expansion-plan.md` is the right
place to pin this with a cross-implementation test: a body carrying an extra key
decodes on both the Rust and C++ servers under the default policy.

**Half of that pinning shipped in #37.** `interop/cpp/generate_fixtures.cpp` emits
JSON and BEVE requests carrying an unknown key, and `tests/interop.rs` decodes them
into a struct that predates the key. The untested direction is a Glaze server
accepting an extra key from repe, which cannot be pinned until Glaze's
`error_on_unknown_keys` default is settled.

## Test Matrix

- Tolerant default (JSON and BEVE): a body with one extra, undeclared key decodes; the
  known fields are present; the extra key is dropped.
- `Reject` policy (JSON): the same body fails with an error that names the offending key
  and its JSON Pointer path; a body with no extra keys still decodes.
- Per-endpoint override: a tolerant server with one `Reject` endpoint, and a strict
  server with one `Tolerate` endpoint, each behave per-endpoint.
- `#[repe(deny_unknown)]`: a struct-registered handler rejects an extra key; an
  unannotated sibling handler tolerates it.
- Nested objects: an extra key inside a nested object follows the same policy as the top
  level.
- Cross-implementation (interop suite): a body with an extra key round-trips through
  both servers under the default policy.

## Exit Criteria

- The protocol and server docs state the default tolerance and the opt-in strictness.
- `UnknownFieldPolicy` is configurable per server and per endpoint, defaulting to
  `Tolerate`, with the existing tolerant path unchanged when unset.
- `Reject` produces a precise, actionable error for JSON bodies.
- The test matrix passes, including a cross-implementation case.

## Open Questions

- **BEVE strictness.** Can the BEVE codec express "error on unknown key" cleanly, or is
  `Reject` JSON-only for a first cut? If JSON-only, the docs must say so plainly.
- **Error shape.** Should `Reject` report only the first unknown key (cheap, matches
  how strict serde fails) or collect all of them (friendlier, more work)? First-key is
  the likely default.
- **Granularity of the derive attribute.** Is struct-level `#[repe(deny_unknown)]`
  enough, or is field-level control (reject unknown keys only within a specific nested
  object) worth the extra surface? Probably defer field-level until asked for.
