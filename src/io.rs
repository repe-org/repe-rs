use crate::{constants::HEADER_SIZE, error::RepeError, header::Header, message::Message};
use std::io::{Read, Write};

/// Read a full REPE message from a stream implementing `Read`.
/// Blocks until the complete header+query+body are read or an error occurs.
pub fn read_message<R: Read>(r: &mut R) -> Result<Message, RepeError> {
    let mut hdr_buf = [0u8; HEADER_SIZE];
    read_exact(r, &mut hdr_buf)?;
    let header = Header::decode(&hdr_buf)?;
    let mut query = vec![0u8; header.query_length as usize];
    if !query.is_empty() {
        read_exact(r, &mut query)?;
    }
    let mut body = vec![0u8; header.body_length as usize];
    if !body.is_empty() {
        read_exact(r, &mut body)?;
    }
    Message::new(header, query, body)
}

/// Read a full REPE message frame into `buf`, reusing its allocation across
/// calls.
///
/// On success `buf` holds the complete wire frame (header + query + body) and
/// can be parsed with [`MessageView::from_slice`](crate::MessageView::from_slice)
/// to dispatch without the per-request query and body `Vec` allocations that
/// [`read_message`] makes. A server reading many requests on one connection
/// keeps one `buf` and reuses it, so steady-state framing is allocation-free.
///
/// `buf` is cleared first; its capacity is retained, so once it has grown to the
/// largest frame seen no further allocation occurs.
pub fn read_message_into<R: Read>(r: &mut R, buf: &mut Vec<u8>) -> Result<(), RepeError> {
    buf.clear();
    buf.resize(HEADER_SIZE, 0);
    read_exact(r, &mut buf[..HEADER_SIZE])?;
    let header = Header::decode(&buf[..HEADER_SIZE])?;
    let total = HEADER_SIZE + header.query_length as usize + header.body_length as usize;
    buf.resize(total, 0);
    read_exact(r, &mut buf[HEADER_SIZE..total])?;
    Ok(())
}

/// Write a full REPE message to a stream implementing `Write`.
pub fn write_message<W: Write>(w: &mut W, msg: &Message) -> Result<(), RepeError> {
    let header_bytes = msg.header.encode();
    w.write_all(&header_bytes)?;
    if !msg.query.is_empty() {
        w.write_all(&msg.query)?;
    }
    if !msg.body.is_empty() {
        w.write_all(&msg.body)?;
    }
    Ok(())
}

/// Emit a REPE message whose body is produced by `body_writer` rather than
/// owned by a `Message` value.
///
/// The caller supplies the wire body length (`body_len`) up front; this lets
/// the header be written before the body and avoids buffering the body into
/// a `Vec<u8>` first. `header.query_length`, `header.body_length`, and
/// `header.length` are overwritten to reflect `query.len()` and `body_len`,
/// so the caller does not need to set them before calling this function.
///
/// The `body_writer` closure must emit exactly `body_len` bytes; failing to
/// do so produces an unframed message on the wire and the receiving side will
/// either block (short body) or misinterpret subsequent frames (long body).
///
/// `body_writer`'s error type only has to be convertible into [`RepeError`], so
/// a raw `w.write_all(..)` (yielding [`std::io::Error`] → [`RepeError::Io`]) and
/// a streaming encode such as `beve::to_writer_streaming` (yielding
/// [`beve::Error`] → [`RepeError::Beve`]) both work without wrapping, and the
/// failure keeps its original error variant.
///
/// Because REPE frames are length-prefixed, `body_len` has to be known before
/// the body is written. Three patterns supply a serialized body without
/// building a full wire-frame `Vec`:
///
/// * **Reusable scratch buffer** (recommended for a writer sending many
///   frames): serialize the body once into a caller-owned `Vec<u8>` with
///   [`beve::to_vec_into`], which reuses the buffer's allocation and clears it
///   on each call, then pass `scratch.len()` as `body_len` and
///   `|w| w.write_all(&scratch)` as the writer. One encode per message and,
///   once the buffer has grown to fit the largest body, no further allocation.
///   See `examples/beve_streaming_body.rs`.
/// * **Known-length raw body**: when the length is already known (a file
///   chunk, a pre-sized buffer), pass it directly and have `body_writer` emit
///   the bytes with `w.write_all(..)` — no body buffer at all.
/// * **Zero-buffer streaming** (for a body too large to hold in memory):
///   measure the encoded length with [`beve::serialized_size`], then stream the
///   body straight to the sink with [`beve::to_writer_streaming`] inside
///   `body_writer` — the body is never materialized as a `Vec`:
///
///   ```no_run
///   # use repe::{Header, RepeError, write_message_streaming};
///   # use serde::Serialize;
///   # fn frame<W: std::io::Write, T: Serialize>(w: &mut W, header: Header, query: &[u8], value: &T) -> Result<(), RepeError> {
///   let body_len = beve::serialized_size(value)?;
///   write_message_streaming(w, header, query, body_len, |w| {
///       beve::to_writer_streaming(w, value)
///   })
///   # }
///   ```
///
/// The zero-buffer path costs a size pass over the value before the write pass.
/// That pass allocates nothing and moves no bytes (each leaf is an integer add):
/// it is O(1) for a `&[u8]`/`serde_bytes` blob or a `beve::TypedSlice<T>`, but
/// O(payload) for a bare numeric `Vec<T>` or a nested structure, which beve
/// walks element by element. Prefer it only when not holding the whole body at
/// once outweighs that second traversal; otherwise the reusable scratch buffer
/// is a single encode. One caveat on error fidelity: beve owns the sink during a
/// streaming encode and folds any write failure into a `beve::Error`, so a
/// mid-body sink error surfaces as [`RepeError::Beve`] rather than
/// [`RepeError::Io`] — the scratch buffer keeps that distinction, since its
/// `write_all` goes straight through repe.
pub fn write_message_streaming<W, F, E>(
    w: &mut W,
    mut header: Header,
    query: &[u8],
    body_len: u64,
    body_writer: F,
) -> Result<(), RepeError>
where
    W: Write,
    F: FnOnce(&mut W) -> Result<(), E>,
    E: Into<RepeError>,
{
    header.query_length = query.len() as u64;
    header.body_length = body_len;
    header.length = (HEADER_SIZE as u64) + header.query_length + body_len;
    w.write_all(&header.encode())?;
    if !query.is_empty() {
        w.write_all(query)?;
    }
    body_writer(w).map_err(Into::into)?;
    Ok(())
}

/// Frame a REPE message whose body is a contiguous numeric slice, encoded as a
/// BEVE typed array straight to the sink with no intermediate body buffer.
///
/// This is the whole-body streaming fast path for a large numeric payload: the
/// body length is computed in closed form with [`beve::typed_slice_size`] (O(1),
/// no traversal) and the payload is written by [`beve::to_writer_typed_slice`]
/// (a single `write_all` of the slice's bytes on little-endian targets). So
/// framing a multi-MiB `&[f64]` costs a header write plus one bulk write, versus
/// the two element-by-element walks (size, then encode) a serde body would take.
///
/// `header.body_format` is set to [`BodyFormat::Beve`]; `query_length`,
/// `body_length`, and `length` are filled in from `query` and the slice (as in
/// [`write_message_streaming`]). The bytes on the wire are identical to a
/// [`MessageBuilder::body_typed_slice`] message written with [`write_message`];
/// decode them with [`Message::decode_typed_slice`].
///
/// For an owned message, or for a complex body (beve has no streaming complex
/// writer yet), build a [`Message`] with
/// [`body_typed_slice`](crate::message::MessageBuilder::body_typed_slice) /
/// [`body_complex_slice`](crate::message::MessageBuilder::body_complex_slice) and
/// frame it with [`write_message`].
///
/// [`BodyFormat::Beve`]: crate::constants::BodyFormat::Beve
/// [`MessageBuilder::body_typed_slice`]: crate::message::MessageBuilder::body_typed_slice
/// [`MessageBuilder`]: crate::message::MessageBuilder
pub fn write_message_typed_slice<W, T>(
    w: &mut W,
    mut header: Header,
    query: &[u8],
    slice: &[T],
) -> Result<(), RepeError>
where
    W: Write,
    T: beve::BeveTypedSlice,
{
    header.body_format = crate::constants::BodyFormat::Beve as u16;
    let body_len = beve::typed_slice_size(slice);
    write_message_streaming(w, header, query, body_len, |w| {
        beve::to_writer_typed_slice(w, slice)
    })
}

fn read_exact<R: Read>(r: &mut R, mut buf: &mut [u8]) -> Result<(), RepeError> {
    while !buf.is_empty() {
        let n = r.read(buf)?;
        if n == 0 {
            return Err(RepeError::Io(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        let tmp = buf;
        buf = &mut tmp[n..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{BodyFormat, QueryFormat};
    use std::io::Cursor;

    #[test]
    fn read_write_roundtrip() {
        let msg = Message::builder()
            .id(7)
            .query_str("/echo")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"x": 1}))
            .unwrap()
            .build();

        // Write to a buffer
        let mut buf = Vec::new();
        {
            let mut w = Cursor::new(&mut buf);
            write_message(&mut w, &msg).unwrap();
        }

        // Read back
        let mut r = Cursor::new(buf);
        let parsed = read_message(&mut r).unwrap();
        assert_eq!(parsed.header.id, 7);
        assert_eq!(parsed.header.query_format, QueryFormat::JsonPointer as u16);
        assert_eq!(parsed.header.body_format, BodyFormat::Json as u16);
        let v: serde_json::Value = parsed.json_body().unwrap();
        assert_eq!(v["x"], 1);
    }

    #[test]
    fn write_message_streaming_matches_write_message() {
        let body_bytes = vec![0xABu8; 4096];
        let reference = Message::builder()
            .id(99)
            .query_str("/collect/file_chunk")
            .query_format(QueryFormat::JsonPointer)
            .body_bytes(body_bytes.clone())
            .body_format(BodyFormat::Beve)
            .build();

        let mut expected = Vec::new();
        write_message(&mut expected, &reference).unwrap();

        let mut header = Header::new();
        header.id = 99;
        header.query_format = QueryFormat::JsonPointer as u16;
        header.body_format = BodyFormat::Beve as u16;
        let query = b"/collect/file_chunk";

        let mut got = Vec::new();
        write_message_streaming(&mut got, header, query, body_bytes.len() as u64, |w| {
            w.write_all(&body_bytes)
        })
        .unwrap();

        assert_eq!(got, expected);
    }

    #[test]
    fn streaming_beve_body_via_reused_scratch() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct SensorFrame {
            id: u64,
            samples: Vec<f64>,
        }

        let query = b"/ingest/frame";
        // One scratch buffer for the writer; reused across every frame.
        let mut scratch = Vec::new();

        let big = SensorFrame {
            id: 1,
            samples: vec![1.25; 4096],
        };

        // Serialize the body once into the caller-owned buffer, then frame it.
        beve::to_vec_into(&mut scratch, &big).unwrap();
        let mut header = Header::new();
        header.id = big.id;
        header.query_format = QueryFormat::JsonPointer as u16;
        header.body_format = BodyFormat::Beve as u16;
        let mut streamed = Vec::new();
        write_message_streaming(&mut streamed, header, query, scratch.len() as u64, |w| {
            w.write_all(&scratch)
        })
        .unwrap();

        // The streamed frame is byte-identical to the fully-buffered Message.
        let reference = Message::builder()
            .id(big.id)
            .query_bytes(query.to_vec())
            .query_format(QueryFormat::JsonPointer)
            .body_bytes(beve::to_vec(&big).unwrap())
            .body_format(BodyFormat::Beve)
            .build()
            .to_vec();
        assert_eq!(streamed, reference);

        // ...and round-trips back to the original value.
        let parsed = Message::from_slice(&streamed).unwrap();
        let decoded: SensorFrame = parsed.beve_body().unwrap();
        assert_eq!(decoded, big);

        // A subsequent smaller body reuses the buffer: `to_vec_into` clears but
        // keeps capacity, so no reallocation once it has grown to the largest
        // body seen.
        let cap_after_big = scratch.capacity();
        let small = SensorFrame {
            id: 2,
            samples: vec![0.0; 16],
        };
        beve::to_vec_into(&mut scratch, &small).unwrap();
        assert!(
            scratch.capacity() <= cap_after_big,
            "smaller body must reuse the scratch allocation, not grow it"
        );
        assert!(scratch.len() < cap_after_big);
    }

    #[test]
    fn streaming_beve_body_zero_buffer_via_serialized_size() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct SensorFrame {
            id: u64,
            samples: Vec<f64>,
        }

        let query = b"/ingest/frame";
        let frame = SensorFrame {
            id: 7,
            samples: vec![2.5; 4096],
        };

        // Measure the encoded length up front, then stream the body straight to
        // the sink -- the body is never materialized as a `Vec`.
        let body_len = beve::serialized_size(&frame).unwrap();
        let mut header = Header::new();
        header.id = frame.id;
        header.query_format = QueryFormat::JsonPointer as u16;
        header.body_format = BodyFormat::Beve as u16;
        let mut streamed = Vec::new();
        // The body_writer returns beve::Result directly -- its error type only
        // needs to be Into<RepeError>, so no manual error mapping.
        write_message_streaming(&mut streamed, header, query, body_len, |w| {
            beve::to_writer_streaming(w, &frame)
        })
        .unwrap();

        // The core framing contract: serialized_size must predict exactly what
        // to_writer_streaming emits, so the advertised body_length matches the
        // bytes actually written. A wrong prediction would desync the wire.
        let mut streamed_body = Vec::new();
        beve::to_writer_streaming(&mut streamed_body, &frame).unwrap();
        assert_eq!(body_len, streamed_body.len() as u64);

        let parsed = Message::from_slice(&streamed).unwrap();
        assert_eq!(parsed.header.body_length, body_len);
        assert_eq!(parsed.body, streamed_body);

        // Streaming-encoded bytes round-trip through the normal (non-streaming)
        // decoder back to the original value.
        let decoded: SensorFrame = parsed.beve_body().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn streaming_body_writer_error_keeps_repe_error_variant() {
        // A beve error raised inside body_writer surfaces as RepeError::Beve,
        // not a flattened RepeError::Io.
        let mut sink = Vec::new();
        let beve_err = write_message_streaming(&mut sink, Header::new(), b"/x", 3, |_w| {
            Err(beve::Error::msg("encode failed"))
        })
        .unwrap_err();
        assert!(
            matches!(beve_err, RepeError::Beve(_)),
            "expected RepeError::Beve, got {beve_err:?}"
        );

        // And an io::Error keeps RepeError::Io with its ErrorKind intact.
        let mut sink = Vec::new();
        let io_err = write_message_streaming(&mut sink, Header::new(), b"/x", 3, |_w| {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        })
        .unwrap_err();
        match io_err {
            RepeError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            other => panic!("expected RepeError::Io, got {other:?}"),
        }
    }

    #[test]
    fn write_message_streaming_with_empty_query_and_body() {
        let header = Header::new();
        let mut got = Vec::new();
        write_message_streaming(&mut got, header, &[], 0, |w| w.write_all(b"")).unwrap();

        let parsed = Message::from_slice(&got).unwrap();
        assert_eq!(parsed.header.length, HEADER_SIZE as u64);
        assert!(parsed.query.is_empty());
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn read_message_unexpected_eof() {
        let header = Header::new();
        let mut partial = header.encode().to_vec();
        partial.truncate(HEADER_SIZE - 4);
        let mut r = Cursor::new(partial);
        let err = read_message(&mut r).unwrap_err();
        match err {
            RepeError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            _ => panic!("unexpected err {err:?}"),
        }
    }
}
