use crate::async_io::read_message_into_async;
use crate::constants::HEADER_SIZE;
use crate::error::RepeError;
use crate::message::{Message, MessageView};
use crate::server::Router;
use crate::server_request::route_request_view;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::time::{Duration, timeout};

pub struct AsyncServer {
    router: Router,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl AsyncServer {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            read_timeout: None,
            write_timeout: None,
        }
    }
    pub fn read_timeout(mut self, d: Option<Duration>) -> Self {
        self.read_timeout = d;
        self
    }
    pub fn write_timeout(mut self, d: Option<Duration>) -> Self {
        self.write_timeout = d;
        self
    }

    pub async fn listen<A: ToSocketAddrs>(addr: A) -> std::io::Result<TcpListener> {
        TcpListener::bind(addr).await
    }

    pub async fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        loop {
            let (stream, _addr) = listener.accept().await?;
            // Disable Nagle on the response path: a buffered response is flushed
            // as one write, so coalescing it with delayed-ACK only adds latency,
            // most visibly on large multi-segment numeric bodies. The async
            // client already sets this on its side (`AsyncClient::connect`); this
            // matches it on the accept side. A failure here is non-fatal (the
            // socket still works, just with Nagle on), so it is not propagated.
            let _ = stream.set_nodelay(true);
            let router = self.router.clone();
            let rt = self.read_timeout;
            let wt = self.write_timeout;
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, router, rt, wt).await {
                    eprintln!("[repe] async connection error: {e}");
                }
            });
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    router: Router,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> Result<(), RepeError> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);
    // Reused across every request on this connection so steady-state framing
    // allocates nothing: each frame is parsed as a borrowed `MessageView` and
    // the response echoes the query straight out of `buf`.
    let mut buf = Vec::new();
    loop {
        if let Some(dur) = read_timeout {
            match timeout(dur, read_message_into_async(&mut reader, &mut buf)).await {
                Ok(r) => r?,
                Err(_) => return Ok(()),
            }
        } else {
            read_message_into_async(&mut reader, &mut buf).await?;
        }

        let view = MessageView::from_slice(&buf)?;
        if let Some(resp) = route_request_view(&router, &view) {
            // Echo the request query (a borrowed slice of `buf`) unless the
            // handler set its own; no response query buffer either way.
            let echo = crate::message::response_echo_query(&resp, view.query);
            if let Some(dur) = write_timeout {
                timeout(dur, write_view_response(&mut writer, &resp, echo))
                    .await
                    .ok();
                timeout(dur, writer.flush()).await.ok();
            } else {
                write_view_response(&mut writer, &resp, echo).await?;
                writer.flush().await?;
            }
        }
    }
}

/// Write a query-less response framed with an externally supplied (borrowed)
/// query, the async counterpart of the TCP server's `write_message_streaming`
/// call. Patches the header's `query_length`/`length` to match `query`.
async fn write_view_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &Message,
    query: &[u8],
) -> Result<(), RepeError> {
    let mut header = resp.header;
    header.query_length = query.len() as u64;
    header.length = HEADER_SIZE as u64 + header.query_length + header.body_length;
    writer.write_all(&header.encode()).await?;
    if !query.is_empty() {
        writer.write_all(query).await?;
    }
    if !resp.body.is_empty() {
        writer.write_all(&resp.body).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_client::AsyncClient;

    #[tokio::test(flavor = "current_thread")]
    async fn async_server_with_async_client() {
        let router = Router::new().with_json("/mul", |v: serde_json::Value| {
            let a = v.get("a").and_then(|x| x.as_i64()).unwrap_or(0);
            let b = v.get("b").and_then(|x| x.as_i64()).unwrap_or(0);
            Ok(serde_json::json!({"prod": a * b}))
        });

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accept one connection in background and handle it
        let router_clone = router.clone();
        let accept_handle = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.unwrap();
            let _ = handle_connection(stream, router_clone, None, None).await;
        });

        let client = AsyncClient::connect(addr).await.unwrap();
        let out = client
            .call_json("/mul", &serde_json::json!({"a": 6, "b": 7}))
            .await
            .unwrap();
        assert_eq!(out["prod"], 42);

        // Drop client to close connection; handler future should finish
        drop(client);
        accept_handle.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_read_timeout_closes_connection() {
        let router = Router::new();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Start accept + handle with a short read timeout
        let router_clone = router.clone();
        let accept_handle = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.unwrap();
            // 20ms timeout: with no client writes, the handler should exit quickly
            let _ = handle_connection(stream, router_clone, Some(Duration::from_millis(20)), None)
                .await;
        });

        // Connect but do not send any data
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Await server task; if it hangs, test will time out (but should finish)
        accept_handle.await.unwrap();
    }
}
