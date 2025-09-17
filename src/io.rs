use crate::{constants::HEADER_SIZE, error::RepeError, header::Header, message::Message};
use std::io::{Read, Write};

/// Read a full REPE message from a stream implementing `Read`.
/// Blocks until the complete header+query+body are read or an error occurs.
pub fn read_message<R: Read>(r: &mut R) -> Result<Message, RepeError> {
    let mut hdr_buf = [0u8; HEADER_SIZE];
    read_exact(r, &mut hdr_buf)?;
    let header = Header::decode(&hdr_buf)?;
    let total = header.query_length as usize + header.body_length as usize;
    let mut rest = vec![0u8; total];
    if total > 0 {
        read_exact(r, &mut rest)?;
    }
    let mut full = Vec::with_capacity(HEADER_SIZE + total);
    full.extend_from_slice(&hdr_buf);
    full.extend_from_slice(&rest);
    Message::from_slice(&full)
}

/// Write a full REPE message to a stream implementing `Write`.
pub fn write_message<W: Write>(w: &mut W, msg: &Message) -> Result<(), RepeError> {
    let bytes = msg.to_vec();
    w.write_all(&bytes)?;
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
