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
/// Pairs with `beve::to_writer_streaming` so that a large binary body can be
/// BEVE-encoded directly into the caller-supplied writer with zero
/// intermediate `Vec<u8>`.
pub fn write_message_streaming<W, F>(
    w: &mut W,
    mut header: Header,
    query: &[u8],
    body_len: u64,
    body_writer: F,
) -> Result<(), RepeError>
where
    W: Write,
    F: FnOnce(&mut W) -> std::io::Result<()>,
{
    header.query_length = query.len() as u64;
    header.body_length = body_len;
    header.length = (HEADER_SIZE as u64) + header.query_length + body_len;
    w.write_all(&header.encode())?;
    if !query.is_empty() {
        w.write_all(query)?;
    }
    body_writer(w)?;
    Ok(())
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
    fn write_message_streaming_with_empty_query_and_body() {
        let header = Header::new();
        let mut got = Vec::new();
        write_message_streaming(&mut got, header, &[], 0, |_| Ok(())).unwrap();

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
