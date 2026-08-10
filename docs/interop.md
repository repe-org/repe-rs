# C++ Interoperability

`repe` speaks the same wire protocol as the canonical C++ implementation,
[Glaze](https://github.com/stephenberry/glaze). A frame written by one is parsed
by the other, byte for byte. This page describes the guarantee and how it is
enforced.

## What is guaranteed

Both implementations target REPE **version 1**: the fixed **48-byte** header
(`length, spec = 0x1507, version = 1, notify, reserved, id, query_length,
body_length, query_format, body_format, ec`), little-endian, followed by the
query and body. The compatibility surface this crate pins against Glaze:

- **Header packing** — every field at the same offset and width.
- **Query** — JSON Pointer (`query_format = 1`) and raw (`0`).
- **JSON bodies** — `body_format = 2`, decoded to the same value.
- **BEVE bodies** — `body_format = 1`, both objects and typed numeric arrays.
  The numeric typed-array layout produced by this crate's
  [`body_typed_slice`](numeric-bodies.md) is byte-identical to Glaze's BEVE
  encoding of the same array.
- **Error responses** — `ec` set, `body_format = 3` (UTF-8), the message in the
  body. Error codes map per the REPE table (e.g. `6` = method not found).
- **Notify** — the `notify` flag round-trips.

## How it is enforced

The enforcement is deterministic and needs no C++ toolchain to run the tests.

1. A small C++ generator (`interop/cpp/`) links Glaze and emits authentic REPE
   frames — it is the only producer of the bytes.
2. Those frames and a manifest describing each one are committed under
   `interop/fixtures/`.
3. `tests/interop.rs` loads each fixture and checks four tiers:
   - **Parse parity** — `Message::from_slice` reproduces every header field and
     the query.
   - **Body decode** — the body decodes to the expected value (JSON, BEVE
     object, BEVE typed numeric, or UTF-8 error).
   - **Byte-identity round-trip** — re-encoding the parsed message reproduces the
     original frame exactly.
   - **Encoder parity from scratch** — for fixtures whose layout is fully
     protocol-defined, the message is rebuilt from logical content alone and
     must equal the Glaze bytes.

A note on JSON: REPE does not fix JSON whitespace, object key order, or number
formatting, so the suite does **not** demand byte-identical JSON output from
`serde_json`. JSON parity is covered semantically (decoded-value equality and
round-trip) rather than by from-scratch byte identity. BEVE numeric arrays, error
frames, and header/query-only frames *are* byte-defined and are checked from
scratch.

The committed fixtures are generated against a pinned Glaze tag, and the
`interop` CI job rebuilds the generator from that same tag, regenerates, and
fails on any diff — so neither a Glaze change nor a repe-rs change can break
compatibility unnoticed. To regenerate locally, see
[`interop/README.md`](https://github.com/repe-org/repe-rs/blob/main/interop/README.md).

## Schema evolution (unknown body keys)

REPE recommends that a server ignore unknown object keys in a structured request body and decode the rest, so a newer client's optional field degrades gracefully against an older server (see [Schema Evolution](protocol.md#schema-evolution)). repe-rs does this by default on every request-decode path; `tests/unknown_fields.rs` pins it end-to-end in Rust.

One direction of this is pinned cross-implementation. The `json_request_unknown_key` and `beve_request_unknown_key` fixtures are Glaze-authored request bodies that carry object keys an older schema never declared — an interleaved scalar (`region`, between two known fields) and a trailing nested object (`meta`). `tests/interop.rs` decodes each into the older Rust struct and asserts the unknown keys are ignored rather than rejected, over both JSON and BEVE (where skipping an unknown key, and a whole unknown sub-object, is the non-trivial binary-format path).

The reverse direction — a repe-produced body with an extra key decoding on a Glaze *server* under its default policy — is still follow-up. It needs a running Glaze server (the fixture generator only emits bytes, it does not receive them) and depends on the C++ side defaulting its REPE server to ignore unknown keys; Glaze's `error_on_unknown_keys` is strict by default today. That is the ecosystem-level half of the same recommendation.

## A note on BEVE variants

The guarantee above covers BEVE **objects** and **typed numeric arrays**, which is what the fixtures exercise. It does not cover a sum type — a Rust `enum` against a C++ `std::variant`. That is a gap in coverage, not a known incompatibility, and it is worth knowing why.

The BEVE specification has its own version line, independent of REPE's. BEVE **Version 2** deprecates the type-tag extension that Version 1 used to mark a variant, and writes one as ordinary self-describing data instead. Both implementations have made that move — the `beve` crate in its 5.0 release, Glaze in 8.0.0 — and both sides of this suite are now past it: repe depends on `beve 7`, and the fixtures are generated from Glaze v8.0.0. A peer older than either writes and expects the Version 1 form; decoding is backward compatible and encoding is not, so an older peer's variant is read correctly here while one this crate writes is not.

Version 2 alone does not make the two sides agree, because it fixes the encoding but not the *shape*, and each language picks that per type. Rust's serde defaults to external tagging (`{"Name": payload}`); a plain C++ `std::variant` is written bare, which corresponds to `#[serde(untagged)]`; a Glaze variant with `tag`/`ids` is internally tagged, which corresponds to `#[serde(tag = "...")]`. Pair them deliberately — the encoding is only half the contract.

None of this is pinned by the fixture suite, so nothing here fails loudly if it drifts. Until it is, either pair the shapes deliberately and test the pairing yourself, or sidestep it: model the case as an object with an explicit discriminant field, or send it over JSON. Structs, numbers, strings, containers, and typed numeric arrays are unaffected either way, and are what the fixtures cover.

## Versioning note (v1 vs v2)

The [REPE specification](https://github.com/repe-org/REPE) has a work-in-progress
**version 2** (a 32-byte header with the `spec` magic moved to the front and no
leading `length` field), but it lives on a branch and has not been released.
Version 1 is the shipping spec: this crate (`REPE_VERSION = 1`) and the released
Glaze implementation both target it and are wire-compatible with each other.
Adopting v2 once it lands is coordinated future work across the spec, Glaze, and
this crate; this interop suite pins the v1 reality that ships today.
