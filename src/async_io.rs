use crate::{constants::HEADER_SIZE, error::RepeError, header::Header, message::Message};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn read_message_async<R: AsyncRead + Unpin>(r: &mut R) -> Result<Message, RepeError> {
    let mut hdr = [0u8; HEADER_SIZE];
    r.read_exact(&mut hdr).await?;
    let header = Header::decode(&hdr)?;
    let mut query = vec![0u8; header.query_length as usize];
    if !query.is_empty() {
        r.read_exact(&mut query).await?;
    }
    let mut body = vec![0u8; header.body_length as usize];
    if !body.is_empty() {
        r.read_exact(&mut body).await?;
    }
    Message::new(header, query, body)
}

/// Async counterpart of [`read_message_into`](crate::read_message_into): read a
/// full REPE frame into `buf`, reusing its allocation across calls. On success
/// `buf` holds the complete wire frame; parse it with
/// [`MessageView::from_slice`](crate::MessageView::from_slice) to dispatch
/// without per-request query/body allocations.
pub async fn read_message_into_async<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<(), RepeError> {
    buf.clear();
    buf.resize(HEADER_SIZE, 0);
    r.read_exact(&mut buf[..HEADER_SIZE]).await?;
    let header = Header::decode(&buf[..HEADER_SIZE])?;
    let total = HEADER_SIZE + header.query_length as usize + header.body_length as usize;
    buf.resize(total, 0);
    r.read_exact(&mut buf[HEADER_SIZE..total]).await?;
    Ok(())
}

pub async fn write_message_async<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg: &Message,
) -> Result<(), RepeError> {
    let header_bytes = msg.header.encode();
    w.write_all(&header_bytes).await?;
    if !msg.query.is_empty() {
        w.write_all(&msg.query).await?;
    }
    if !msg.body.is_empty() {
        w.write_all(&msg.body).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{BodyFormat, QueryFormat};

    #[tokio::test]
    async fn async_read_write_roundtrip() {
        let (mut a, b) = tokio::io::duplex(4096);
        let msg = Message::builder()
            .id(9)
            .query_str("/ping")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"hello": "world"}))
            .unwrap()
            .build();

        // Write on one end, read on the other
        let w = write_message_async(&mut a, &msg);
        let r = async {
            let mut reader = b;
            read_message_async(&mut reader).await
        };
        let (wres, rres) = tokio::join!(w, r);
        wres.unwrap();
        let parsed = rres.unwrap();
        assert_eq!(parsed.header.id, 9);
        assert_eq!(parsed.header.body_format, BodyFormat::Json as u16);
    }
}
