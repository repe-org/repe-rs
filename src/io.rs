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
/// a raw `w.write_all(..)` and a streaming encode such as
/// [`structio::to_beve_writer`] — which reports [`std::io::Error`], because a
/// BEVE *write* cannot fail on content — both work without wrapping, and the
/// failure keeps its original error variant.
///
/// Because REPE frames are length-prefixed, `body_len` has to be known before
/// the body is written. Three patterns supply a serialized body without
/// building a full wire-frame `Vec`:
///
/// * **Reusable scratch buffer** (recommended for a writer sending many
///   frames): serialize the body once into a caller-owned `Vec<u8>` with
///   [`structio::write_beve_into`], which reuses the buffer's allocation and
///   clears it on each call, then pass `scratch.len()` as `body_len` and
///   `|w| w.write_all(&scratch)` as the writer. One encode per message and,
///   once the buffer has grown to fit the largest body, no further allocation.
///   See `examples/beve_streaming_body.rs`.
/// * **Known-length raw body**: when the length is already known (a file
///   chunk, a pre-sized buffer), pass it directly and have `body_writer` emit
///   the bytes with `w.write_all(..)` — no body buffer at all.
/// * **Zero-buffer streaming** (for a body too large to hold in memory):
///   measure the encoded length with [`structio::beve_size`], then stream the
///   body straight to the sink with [`structio::to_beve_writer`] inside
///   `body_writer` — the body is never materialized as a `Vec`:
///
///   ```no_run
///   # use repe::{Header, RepeError, write_message_streaming};
///   # fn frame<W: std::io::Write, T: structio::beve::Write>(w: &mut W, header: Header, query: &[u8], value: &T) -> Result<(), RepeError> {
///   let body_len = structio::beve_size(value) as u64;
///   write_message_streaming(w, header, query, body_len, |w| {
///       structio::to_beve_writer(value, w)
///   })
///   # }
///   ```
///
/// The zero-buffer path costs a size pass over the value before the write pass.
/// That pass allocates nothing and moves no bytes (each leaf is an integer add):
/// it is O(1) for a `&[u8]` blob or a numeric slice, whose encoded length is
/// arithmetic on the element count, but O(payload) for a nested structure, which
/// is walked member by member. Prefer it only when not holding the whole body at
/// once outweighs that second traversal; otherwise the reusable scratch buffer
/// is a single encode.
///
/// Error fidelity is no longer a caveat: a structio write cannot fail on
/// content, so the only failure a streaming encode can report is the sink's own
/// [`std::io::Error`], and it arrives as [`RepeError::Io`] exactly as the
/// scratch buffer's `write_all` does.
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
/// body length is computed in closed form with [`structio::beve_size`] (O(1),
/// no traversal) and the payload is written by [`structio::to_beve_writer`]
/// (a single `write_all` of the slice's bytes on little-endian targets). So
/// framing a multi-MiB `&[f64]` costs a header write plus one bulk write, versus
/// the two element-by-element walks (size, then encode) a member-wise body takes.
///
/// `header.body_format` is set to [`BodyFormat::Beve`]; `query_length`,
/// `body_length`, and `length` are filled in from `query` and the slice (as in
/// [`write_message_streaming`]). The bytes on the wire are identical to a
/// [`MessageBuilder::body_typed_slice`] message written with [`write_message`];
/// decode them with [`Message::decode_typed_slice`].
///
/// For a complex body use the sibling [`write_message_complex_slice`]. For an
/// owned message, build a [`Message`] with
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
    T: structio::beve::NumericBytes + structio::beve::Write,
{
    header.body_format = crate::constants::BodyFormat::Beve as u16;
    let body_len = structio::beve_size(slice) as u64;
    write_message_streaming(w, header, query, body_len, |w| {
        structio::to_beve_writer(slice, w)
    })
}

/// Frame a REPE message whose entire body is a complex numeric array, writing the
/// BEVE complex extension array straight to the sink with no intermediate body
/// buffer — the complex counterpart of [`write_message_typed_slice`].
///
/// The body length is computed in closed form with [`structio::beve_size`]
/// (O(1), no traversal) and the payload is written by
/// [`structio::to_beve_writer`] (a single `write_all` of the interleaved
/// `(re, im)` bytes on little-endian targets). So framing a large
/// `&[Complex<f64>]` costs a header write plus one bulk write, versus building and
/// allocating the whole body up front via
/// [`body_complex_slice`](crate::message::MessageBuilder::body_complex_slice).
///
/// `header.body_format` is set to [`BodyFormat::Beve`]; `query_length`,
/// `body_length`, and `length` are filled in from `query` and the slice (as in
/// [`write_message_streaming`]). The bytes on the wire are identical to a
/// [`MessageBuilder::body_complex_slice`] message written with [`write_message`];
/// decode them with [`Message::decode_complex_slice`].
///
/// [`BodyFormat::Beve`]: crate::constants::BodyFormat::Beve
/// [`MessageBuilder::body_complex_slice`]: crate::message::MessageBuilder::body_complex_slice
/// [`Message::decode_complex_slice`]: crate::message::Message::decode_complex_slice
pub fn write_message_complex_slice<W, T>(
    w: &mut W,
    mut header: Header,
    query: &[u8],
    slice: &[structio::Complex<T>],
) -> Result<(), RepeError>
where
    W: Write,
    structio::Complex<T>: structio::beve::NumericBytes + structio::beve::Write,
{
    header.body_format = crate::constants::BodyFormat::Beve as u16;
    let body_len = structio::beve_size(slice) as u64;
    write_message_streaming(w, header, query, body_len, |w| {
        structio::to_beve_writer(slice, w)
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

    #[derive(Default, Debug, PartialEq)]
    struct Point {
        x: i32,
    }
    structio::object!(Point { x });

    #[test]
    fn read_write_roundtrip() {
        let msg = Message::builder()
            .id(7)
            .query_str("/echo")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&Point { x: 1 })
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
        let v: Point = parsed.json_body().unwrap();
        assert_eq!(v.x, 1);
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
        #[derive(Default, Debug, PartialEq)]
        struct SensorFrame {
            id: u64,
            samples: Vec<f64>,
        }
        structio::object!(SensorFrame { id, samples });

        let query = b"/ingest/frame";
        // One scratch buffer for the writer; reused across every frame.
        let mut scratch = Vec::new();

        let big = SensorFrame {
            id: 1,
            samples: vec![1.25; 4096],
        };

        // Serialize the body once into the caller-owned buffer, then frame it.
        structio::write_beve_into(&big, &mut scratch);
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
            .body_bytes(structio::to_beve(&big))
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
        structio::write_beve_into(&small, &mut scratch);
        assert!(
            scratch.capacity() <= cap_after_big,
            "smaller body must reuse the scratch allocation, not grow it"
        );
        assert!(scratch.len() < cap_after_big);
    }

    #[test]
    fn streaming_beve_body_zero_buffer_via_serialized_size() {
        #[derive(Default, Debug, PartialEq)]
        struct SensorFrame {
            id: u64,
            samples: Vec<f64>,
        }
        structio::object!(SensorFrame { id, samples });

        let query = b"/ingest/frame";
        let frame = SensorFrame {
            id: 7,
            samples: vec![2.5; 4096],
        };

        // Measure the encoded length up front, then stream the body straight to
        // the sink -- the body is never materialized as a `Vec`.
        let body_len = structio::beve_size(&frame) as u64;
        let mut header = Header::new();
        header.id = frame.id;
        header.query_format = QueryFormat::JsonPointer as u16;
        header.body_format = BodyFormat::Beve as u16;
        let mut streamed = Vec::new();
        // The body_writer returns io::Result directly -- its error type only
        // needs to be Into<RepeError>, so no manual error mapping.
        write_message_streaming(&mut streamed, header, query, body_len, |w| {
            structio::to_beve_writer(&frame, w)
        })
        .unwrap();

        // The core framing contract: `beve_size` must predict exactly what
        // `to_beve_writer` emits, so the advertised body_length matches the
        // bytes actually written. A wrong prediction would desync the wire.
        let mut streamed_body = Vec::new();
        structio::to_beve_writer(&frame, &mut streamed_body).unwrap();
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
        // Whatever variant `body_writer` reports reaches the caller intact,
        // rather than being flattened into `RepeError::Io`. A BEVE *write*
        // cannot fail on content under structio, so what a streaming body
        // reports is a `StreamError`, and the format it is attributed to is the
        // caller's to name — `StreamError` does not carry one.
        let mut sink = Vec::new();
        let beve_err = write_message_streaming(&mut sink, Header::new(), b"/x", 3, |_w| {
            Err(RepeError::decode_stream(
                crate::constants::BodyFormat::Beve,
                structio::StreamError::Parse(structio::Error::new(
                    structio::ErrorCode::ExpectedObject,
                    0,
                )),
            ))
        })
        .unwrap_err();
        assert!(
            matches!(beve_err, RepeError::Beve(_)),
            "expected RepeError::Beve, got {beve_err:?}"
        );

        // The same failure under a JSON frame is a JSON error. A `From` impl
        // could not have told these apart.
        assert!(matches!(
            RepeError::decode_stream(
                crate::constants::BodyFormat::Json,
                structio::StreamError::Parse(structio::Error::new(
                    structio::ErrorCode::ExpectedObject,
                    0,
                )),
            ),
            RepeError::Json(_)
        ));

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
