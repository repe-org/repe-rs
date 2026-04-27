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
