use crate::async_io::{read_message_async, write_message_async};
use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::message::{create_error_response_like, Message};
use crate::server::Router;
use tokio::io::AsyncWriteExt;
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::time::{timeout, Duration};

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
    loop {
        let req: Message = if let Some(dur) = read_timeout {
            match timeout(dur, read_message_async(&mut reader)).await {
                Ok(r) => r?,
                Err(_) => return Ok(()),
            }
        } else {
            read_message_async(&mut reader).await?
        };

        let notify = req.header.notify == 1;
        if req.header.version != REPE_VERSION {
            if !notify {
                let resp = create_error_response_like(
                    &req,
                    ErrorCode::VersionMismatch,
                    format!("Unsupported REPE version {}", req.header.version),
                );
                if let Some(dur) = write_timeout {
                    timeout(dur, write_message_async(&mut writer, &resp))
                        .await
                        .ok();
                    timeout(dur, writer.flush()).await.ok();
                } else {
                    write_message_async(&mut writer, &resp).await?;
                    writer.flush().await?;
                }
            }
            continue;
        }
        let qf = QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary);
        let path = match qf {
            QueryFormat::JsonPointer => req.query_utf8(),
            QueryFormat::RawBinary => {
                if !notify {
                    let resp = create_error_response_like(
                        &req,
                        ErrorCode::InvalidQuery,
                        "Raw binary queries are not supported by this server",
                    );
                    if let Some(dur) = write_timeout {
                        timeout(dur, write_message_async(&mut writer, &resp))
                            .await
                            .ok();
                        timeout(dur, writer.flush()).await.ok();
                    } else {
                        write_message_async(&mut writer, &resp).await?;
                        writer.flush().await?;
                    }
                }
                continue;
            }
        };

        let resp = match router.get(&path) {
            Some(handler) => match handler.handle(&req) {
                Ok(m) => m,
                Err(e) => create_error_response_like(&req, e.to_error_code(), e.to_string()),
            },
            None => create_error_response_like(
                &req,
                ErrorCode::MethodNotFound,
                format!("Method not found: {path}"),
            ),
        };

        if !notify {
            if let Some(dur) = write_timeout {
                timeout(dur, write_message_async(&mut writer, &resp))
                    .await
                    .ok();
                timeout(dur, writer.flush()).await.ok();
            } else {
                write_message_async(&mut writer, &resp).await?;
                writer.flush().await?;
            }
        }
    }
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

        let mut client = AsyncClient::connect(addr).await.unwrap();
        let out = client
            .call_json("/mul", &serde_json::json!({"a": 6, "b": 7}))
            .await
            .unwrap();
        assert_eq!(out["prod"], 42);

        // Drop client to close connection; handler future should finish
        drop(client);
        let _ = accept_handle.await;
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
        let _ = accept_handle.await.unwrap();
    }
}
