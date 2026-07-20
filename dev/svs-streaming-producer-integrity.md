# SVS streaming-producer integrity: a deferred opt-in digest extension

## Status

Deferred by decision, not merely unscheduled. Surfaced while building a **lazy streaming producer** (a server that serializes a large in-memory value on demand as the client pulls, rather than materializing the encoded bytes first). Such a producer cannot participate in SVS's integrity model without either encoding the value twice or giving up end-to-end verification. Closing that gap is a real feature, but committing a digest the consumer verifies against is a **wire-protocol change** (see "What this requires"), and SVS's own discipline argues against building it before the need is proven: v1 is frozen for interop, §5 says "keep this minimal until a second version actually exists," and §7 lists content integrity beyond the transport as an *explicit* non-goal.

The recommendation, in one line: **don't touch the wire now; if a concrete trigger fires, add a small backward-compatible companion route under `/_svs` — not a v2, and not a standalone spec.** Producers that need integrity today use the §7 escape hatch where it fits, and double-encode or size-only where it does not (see "Near-term guidance").

## Background: SVS commits no digest on the wire (by design)

SVS deliberately keeps the transport free of a content digest. Integrity is an out-of-band agreement between producer and consumer:

- A producer is registered with `RouterValueStreamExt::with_value_stream` (a typed `Serialize` value, encoded on pull) or `with_reader_stream` (an opaque `Read`). Both stream lazily and pace to the consumer's pull cadence.
- The consumer recomputes a digest over the streamed bytes in a single pass with `pull_to_file_verified_async(client, resource, path, digest, verify)`: content bytes are tee'd into the caller's `digest` (`TeeWriter`), and `verify(digest)` runs after a clean transfer and before the atomic rename, vetoing publication on mismatch. A truncated transfer surfaces as the pull error and commits nothing.
- The *expected* digest is agreed **out of band** — typically carried in an application manifest the consumer already receives (see `value_stream.rs` module docs and `pull_to_file_verified_async`, and `docs/streaming-serialized-values.md`).

This is a good default: the wire stays minimal, and applications that already ship a manifest get verification for free.

## The gap: a lazy producer has no cheap way to know the digest up front

The out-of-band model assumes the producer *knows the content digest before the pull* so it can put it in the manifest. That holds when the bytes already exist (a file on disk, an in-memory buffer). It does **not** hold for a producer whose whole purpose is to avoid materializing the encoded bytes:

- To pre-declare a digest, the producer must encode the value once **just to hash it** (discarding the bytes), then encode it **again** on pull. That is a full extra serialization pass over a large value.
- The alternative is to drop up-front verification and trust the transport, which is exactly the guarantee the application wanted.

So today a lazy streaming producer must choose between **double-encode CPU** and **no end-to-end integrity**. Neither is forced by the protocol; both fall out of there being no way to commit a digest the producer computes *during* the single streaming pass.

Concretely (generic):

```rust
// Encode #1: only to learn the digest for the manifest.
let mut hasher = Blake3Writer::default();
value.serialize_into(&mut hasher)?;         // full pass, bytes discarded
let (hash, size) = hasher.finish();
manifest.push(FileEntry { resource, hash, size, .. });

// Encode #2: the real stream, on pull.
router.with_value_stream(move |_res| Some(value.clone()), opts);
```

The producer already computes the exact digest cheaply as a by-product of encode #2 — but by then the manifest is long gone to the client.

## What this requires: a wire change, not local policy

It is tempting to frame this as a `StreamOpts` knob (`trailer_digest: Option<DigestKind>`) and treat it as engine-local (§0/§4). The hashing halves are local — one shipped, one a small local addition:

- **Consumer-side tee-and-veto already ships:** `TeeWriter` and `pull_to_file_verified_async` compute a single-pass digest over the streamed bytes and run `verify` after a clean transfer and before the atomic rename.
- **Producer-side hashing while streaming** is a tee at a byte boundary the produce pipeline already crosses. The shipped `with_writer_stream(format, resolve, opts)` seam exposes the `Write` sink so an app can serialize through a hashing tee and append a digest single-pass itself; an *automatic* digest on the `with_value_stream` / typed paths (where the engine owns the encode) would be a small local engine change. Either way it is local, not wire.

The one thing neither side can fake locally is **carrying the producer's digest across to the consumer**, and that is squarely wire. By SVS §0 the normative contract *is* "the message shapes ... and the streaming/termination semantics actually exchanged" — "You cannot get interop by matching engines; you get it by matching the bytes below." A digest the consumer must read and verify is exchanged bytes, and there is no spare slot to hide it in (the REPE header's 4-byte `reserved` field is normatively must-be-zero for senders and far too small for a digest anyway; the terminal `next` query is exactly one byte). It collides with:

- **§2 / §2.2** — the frozen message set is exactly `{open, next, cancel}`, terminating on the 1-byte `last = 1` flag.
- **§3** — "the reassembled concatenation of `next` bodies is *exactly* the producer's byte stream," and "termination is the `last = 1` flag only." A digest smuggled into any `next` body corrupts reconstruction; a frame after `last = 1` redefines termination.
- **§5** — v1 is frozen with "no negotiation beyond the client checking the returned version."
- **§7** — content integrity beyond the transport is an *explicit* non-goal, with the sanctioned answer being to carry the digest inside the serialized value.

So the digest-carriage cannot be added as local policy. That does **not** mean it must be a version bump; it means the carriage must be *designed as wire* when the time comes (see "If built").

## Strategy: defer now; a companion `/_svs` route if triggered — not v2, not a new spec

Three wire homes were weighed. The recommendation is to defer, and to reserve the companion route for when the need is real.

**Not a monolithic v2.** A version bump overrides every deliberate stance at once (§7 non-goal, §5 "keep minimal," frozen v1) to serve a narrow population. It spends the ecosystem's one clean version-bump budget, creates a permanent v1/v2 coexistence matrix, and reopens the core to the push/resume features §7 deliberately kept out. And the digest would still be per-stream-optional *within* v2 — not every producer hashes — so a client negotiates per stream anyway, paying the version break *and* the per-stream capability cost.

**Not a standalone spec — yet.** §7's "define it separately rather than extending SVS" is the right *stance*, but a whole second frozen document, with its own conformance suite and a chain of "see SVS §X" substrate pointers, is not justified by a lone trailing digest. A separate contract earns its keep only if integrity arrives **bundled with the credit-windowed push + resume engine §7 already forecasts**, becoming the first brick of that family. That is plausible, not proven; bank the stance, don't write the document now.

**Defer, with the companion route held in the drawer.** The beneficiary set is a four-way conjunction: needs on-wire integrity, *and* cannot precompute a digest for a manifest, *and* needs a standard BEVE/JSON/UTF-8 tag (so the §7 hatch is closed for it), *and* is CPU-bound enough that a double-encode pass hurts. With one implementation, unresolved framing questions, and no second peer yet to conform against (§6 defines conformance as "two independent implementations agree ... byte-for-byte"), freezing a peer-verified frame now would guess a design into a "frozen for interop" contract with no partner to catch mistakes. Defer — but record a **trigger** (below) so the deferral is a decision, not decay into a silent forever-no that tempts a private, incompatible trailer (the fork `dev/unknown-field-tolerance.md` warns of).

## If built: an optional pulled `/_svs/digest` route

When the trigger fires, the clean shape is a **new optional pulled route in the reserved `/_svs` namespace** (§1: routes under `_` are reserved for REPE extensions) — *not* the in-stream trailer an earlier draft of this note described.

- The client, after receiving `last = 1`, calls `query = /_svs/digest`, body `BodyFormat::Beve` = `{ stream_id: u64 }`. The response body is `{ digest_kind: u8, digest: bytes }` — self-describing, so BLAKE3 is not baked in.
- The digest is computed over the **on-wire byte stream**: the post-compression concatenation of `next` bodies, i.e. exactly what §3 pins as "the producer's byte stream." This is the domain that lets *any* receiver verify regardless of its output choice (write-as-is, decompress-to-file, or decompress-and-deserialize); hashing the *decompressed* value would fail verification for the write-as-is path.
- The consumer reuses the existing tee-and-veto: its single-pass `TeeWriter` digest is compared to the route response under the same veto-before-rename semantics. A missing or mismatched digest fails the pull and commits nothing.

Why a pulled route beats an in-stream trailer:

- **It preserves §3 byte-for-byte.** Content still flows through `open`/`next`/`cancel` unchanged, so the reassembled concatenation stays *exactly* the producer's byte stream. An in-stream trailer would inject non-content bytes into that concatenation and weaken the one invariant §3 pins hardest.
- **It stays native to SVS's client-pull model.** The producer never pushes a post-terminal frame. v1 has no notify-push and no `End` message — termination is `last = 1` — so the earlier draft's `Msg::Trailer`/`Msg::End` vocabulary does not map onto v1 and should be read as illustrative only; the wire mechanism is a *pulled* route.
- **Discovery needs no new negotiation.** An old producer returns route-not-found; the client falls back to out-of-band or size-only. No §5 machinery is invented before it is needed, and a v1 peer that never calls the route is permanently correct.

Two details to settle on paper before anything is frozen:

1. **Digest domain** — fixed as the on-wire post-compression bytes (above). Pin it explicitly, or verification silently diverges across receiver output choices. Note this differs from the near-term `payload || digest` hatch, which hashes the *logical* pre-compression payload (what the app's `Read` yields, before the engine's zstd pass); an app moving from the hatch to the route re-hashes, since the domains differ.
2. **Retention vs. discovery ambiguity** — a producer that frees `stream_id` state at `last = 1` (valid v1 behavior) returns route-not-found indistinguishably from one that does not support digests. Resolve by mandating that a digest-supporting producer **retains the digest until it is pulled or the idle timeout fires** (keeps the `open` bytes frozen), rather than adding a want-flag to the `open` request.

**Scope note.** The route proves the bytes arrived intact end-to-end (producer digest == consumer digest); it is *not* an authenticity/signature mechanism. Applications needing provenance still layer their own signed manifest on top, as today.

## Near-term guidance (no spec change)

- **Raw / application-custom streams (`format = 0` or `≥ 4096`):** use the §7-sanctioned escape hatch — the application frames its byte stream as `payload || digest(payload)`. This works with **existing public APIs**: the producer either supplies a `Read` that hashes as bytes flow and emits the digest at EOF (fed to `with_reader_stream`), or — for a value it serializes on the fly — uses the `with_writer_stream` seam to serialize through a hashing tee straight into the sink and append the digest (single-pass, no `Read`↔`Write` bridge). The consumer does a *plain* pull (`pull_consume_async`) and splits/verifies in its own code — *not* `pull_to_file_verified_async`, which hashes the whole stream (trailer included) and would leave the digest bytes appended on disk. SVS sees nothing new, because those bytes legitimately *are* "the producer's byte stream," so §3 holds byte-for-byte. Single-pass end-to-end integrity today.
- **Standard-tagged streams (`format = 1` BEVE, `2` JSON, `3` UTF-8):** the hatch is closed — appending raw digest bytes breaks the format tag and the clean deserialize §3 requires (trailing data for BEVE/JSON, invalid UTF-8 for a UTF-8 stream). For this sub-slice the honest options today are double-encode (correct, pays the extra pass — served end-to-end by the shipping `pull_to_file_verified_async` against an out-of-band manifest digest) or size-only pre-declaration (catches truncation, which the pull already reports, but not corruption). **Record this gap** rather than leaving it silent; it is exactly what the companion route would close. Note the `with_writer_stream` seam (now shipped) lets an app serialize + hash + append a trailing digest single-pass — but as app-framed `payload || digest` tagged RawBinary (the first bullet), *not* a standard BEVE/JSON/UTF-8 stream. So the residual gap is narrower than "double-encode forced": it is specifically *single-pass integrity while keeping a standard `format` tag*, which still needs the `/_svs/digest` route.

## Trigger to revisit

Both conditions gate the **frozen, canonical** route (condition 2 alone may authorize a non-canonical experimental route — see below):

1. A **second independent SVS implementation** exists, so the peer-verified frame can be conformance-tested against a real partner (§6's own conformance bar: "two independent implementations agree ... byte-for-byte") rather than guessed into a frozen contract.
2. A producer **confirmed CPU-bound** on the double-encode pass that **also needs a standard `format` tag** (so the §7 hatch does not already solve it).

When both hold, ship the `/_svs/digest` route directly — do not route through a v2 or a standalone spec. If the motivating producer is already measured CPU-bound and standard-tagged, condition 2 is satisfied and only the second implementation gates a *frozen* fixture; repe-rs may dogfood an experimental route in the interim, but must not fold it into the canonical SVS fixture suite until a second implementation conforms against it.

## Alternatives considered (no wire change)

- **Double-encode (status quo).** Correct and needs no protocol change, but pays a full extra serialization pass per streamed value — the cost this feature removes.
- **Size-only pre-declaration + trust the transport.** Drops content verification; the length check catches truncation (which the pull already reports) but not corruption. Weaker than what non-streaming producers get.
- **Materialize once, serve from a buffer/temp file.** Recovers an up-front digest but reintroduces the full encoded copy (in RAM, or an extra disk round-trip) that lazy streaming exists to avoid.
- **§7 in-value / app-framed digest (`payload || digest`).** Zero spec change and genuinely single-pass, but only for raw / application-custom streams; it forfeits the standard `format` tag (BEVE/JSON/UTF-8), so it does not serve the standard-tagged sub-slice the companion route targets.

## Open questions (mostly resolved on paper; carried until built)

- **Compressed-chunk interaction.** The digest domain is the compressed on-wire bytes, so hashing is over exactly what is sent; confirm this composes cleanly with pipelined `next` (hash in request order, matching §3's reassembly-by-request-order rule).
- **Opaque reader streams.** `/_svs/digest` should serve the `with_reader_stream` case identically — the domain is the on-wire bytes regardless of `format` — so one route covers both producers.
- **Provenance layering.** Confirm the route is not mistaken for authenticity; a future signed-manifest layer composes on top rather than being subsumed (see Scope note).
