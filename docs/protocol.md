# Wire Format and Message I/O

## Header Layout

Each REPE message begins with a fixed `48`-byte header (`HEADER_SIZE`) followed by the `query` payload and the `body` payload. All numeric fields are little-endian. The header `length` field equals `48 + query_length + body_length` and is validated on parse. The header `reserved` field is validated and must be zero.

`query_format` and `body_format` use the enums in `repe::constants`. The suggested query format is JSON Pointer (`/path/to/resource`).

## Streaming and Zero-Copy I/O

`Message::to_vec` and `Message::from_slice` are the convenient round-trip APIs, but for large bodies (e.g. multi-MiB binary chunks) they add a full-body memcpy you may not want. The streaming APIs let the body flow directly between the wire and a user-supplied encoder/decoder.

```rust
use repe::{write_message_streaming, BodyFormat, Header, QueryFormat};
use std::io::Write;

let mut header = Header::new();
header.id = 99;
header.query_format = QueryFormat::JsonPointer as u16;
header.body_format = BodyFormat::Beve as u16;
let body_len = beve_chunk_size(&chunk);

write_message_streaming(&mut socket, header, b"/collect/file_chunk", body_len, |w| {
    beve::to_writer_streaming(w, &chunk).map_err(std::io::Error::other)
})?;
```

```rust
use repe::MessageView;

let view = MessageView::from_slice(&frame_bytes)?;
let path = view.query_str()?;
// view.body is &[u8] pointing inside frame_bytes; pair with serde_bytes::Bytes<'a>
// on a Deserialize struct to keep the chunk payload borrowed end-to-end.
```

`Message::write_to` and `Message::serialized_len` are the owned-`Message` counterparts: emit a `Message` to a `Write` without going through `to_vec`, or query its wire size in O(1).

## JSON Pointer Paths

Method paths follow [RFC 6901](https://datatracker.ietf.org/doc/html/rfc6901): each `/`-separated segment names a step into the server's value tree, so `/api/v1/counter` reaches the field `counter` inside the object `v1` inside the object `api`. The empty path `""` (or just `/`) refers to the root. Two characters need escaping inside a segment: `~` becomes `~0` and `/` becomes `~1`. Most REPE servers expose a flat namespace where the path is just a function or value name, but registry-backed servers may nest values arbitrarily deep.

Helpers:

- `parse_json_pointer` splits a pointer into unescaped tokens.
- `eval_json_pointer` indexes into a `serde_json::Value` using array indices or object keys.

## Schema Evolution

Structured request bodies evolve. A newer client may add an optional key to a request object before every server it talks to has been rebuilt to understand it, and a REPE link commonly spans two versions at once (a client updated ahead of its server, one peer in a fleet running an older image, an embedded device on a slower cadence than its callers). The safe default for such an unknown, additive key is to ignore it rather than reject the whole request:

- A request body's structured object MAY gain new keys over time. A conforming server SHOULD ignore keys it does not recognize and decode the rest, so that an older server stays interoperable with a newer client. Rejecting the entire request over one unrecognized optional key turns a backward-compatible schema addition into a lock-step deployment (every server must upgrade before any client may send the new field), which is exactly the coupling REPE's self-describing bodies exist to avoid.
- Strict rejection of unknown keys is permitted, but it is an explicit, opt-in server choice and MUST NOT be the default. A server that rejects unknown keys should surface a clear, specific error naming the offending key and its path.
- This applies only to keys *within* a structured body. An unknown body *format code* (the `body_format` header field) remains a hard error, unchanged — see the [registry docs](registry.md).

Ignore-and-continue is the right default because it is safe for *additive, optional* fields, which are the common case. It is not safe for a field that changes the *meaning* of a request rather than merely extending it — a `currency`, a `unit`, an auth `scope`, a `dry_run` flag. Silently dropping such a field lets an older server return a confidently wrong result instead of failing loudly. REPE does not yet specify a mechanism for this second class; the intended direction is a per-field *must-understand* marker (in the spirit of SOAP's `mustUnderstand`, JOSE/COSE's `crit`, and X.509 critical extensions) that lets a sender flag a specific key the receiver MUST reject if it does not recognize it, leaving everything else ignore-and-continue. Until that lands, implementations should default to ignore rather than a blanket per-endpoint reject switch, and a caller that needs a meaning-changing field honored should treat it as a versioned method rather than an unmarked optional key.
