use crate::async_client::AsyncClient;
use crate::error::RepeError;
use crate::message::Message;
use crate::server::Router;
use crate::server_request::route_request;
use futures_util::{SinkExt, StreamExt};
use std::io::ErrorKind;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::{self, Message as WsMessage, http::StatusCode};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

pub struct WebSocketServer {
    router: Router,
}

impl WebSocketServer {
    pub fn new(router: Router) -> Self {
        Self { router }
    }

    pub async fn listen<A: ToSocketAddrs>(addr: A) -> std::io::Result<TcpListener> {
        TcpListener::bind(addr).await
    }

    pub async fn serve<A: ToSocketAddrs>(self, addr: A, path: &str) -> std::io::Result<()> {
        let listener = Self::listen(addr).await?;
        self.serve_listener(listener, path).await
    }

    pub async fn serve_listener(self, listener: TcpListener, path: &str) -> std::io::Result<()> {
        let expected_path = normalize_path(path);

        loop {
            let (stream, _addr) = listener.accept().await?;
            let router = self.router.clone();
            let expected_path = expected_path.clone();
            tokio::spawn(async move {
                match accept_repe_websocket(stream, &expected_path).await {
                    Ok(ws_stream) => {
                        if let Err(err) = handle_connection(ws_stream, router).await {
                            eprintln!("[repe] websocket connection error: {err}");
                        }
                    }
                    Err(err) => {
                        eprintln!("[repe] websocket handshake error: {err}");
                    }
                }
            });
        }
    }
}

pub async fn proxy_connection<S>(
    ws_stream: WebSocketStream<S>,
    upstream: AsyncClient,
) -> Result<(), RepeError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = ws_stream.split();

    loop {
        let frame = match reader.next().await {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Err(websocket_transport_error(err)),
            None => break,
        };

        let request = match decode_request_frame(frame)? {
            FrameAction::Message(message) => message,
            FrameAction::Continue => continue,
            FrameAction::Close => break,
        };

        if let Some(response) = upstream.forward_message(&request).await? {
            writer
                .send(WsMessage::Binary(response.to_vec()))
                .await
                .map_err(websocket_transport_error)?;
        }
    }

    let _ = writer.send(WsMessage::Close(None)).await;
    writer.close().await.map_err(websocket_transport_error)
}

async fn accept_repe_websocket(
    stream: TcpStream,
    expected_path: &str,
) -> Result<WebSocketStream<TcpStream>, RepeError> {
    accept_hdr_async(
        stream,
        WebSocketPathValidator {
            expected: expected_path.to_owned(),
        },
    )
    .await
    .map_err(websocket_transport_error)
}

async fn handle_connection(
    ws_stream: WebSocketStream<TcpStream>,
    router: Router,
) -> Result<(), RepeError> {
    let (mut writer, mut reader) = ws_stream.split();

    loop {
        let frame = match reader.next().await {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Err(websocket_transport_error(err)),
            None => break,
        };

        let request = match decode_request_frame(frame)? {
            FrameAction::Message(message) => message,
            FrameAction::Continue => continue,
            FrameAction::Close => break,
        };

        if let Some(response) = route_request(&router, &request) {
            writer
                .send(WsMessage::Binary(response.to_vec()))
                .await
                .map_err(websocket_transport_error)?;
        }
    }

    let _ = writer.send(WsMessage::Close(None)).await;
    writer.close().await.map_err(websocket_transport_error)
}

enum FrameAction {
    Message(Message),
    Continue,
    Close,
}

fn decode_request_frame(frame: WsMessage) -> Result<FrameAction, RepeError> {
    match frame {
        WsMessage::Binary(payload) => Message::from_slice_exact(&payload).map(FrameAction::Message),
        WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => Ok(FrameAction::Continue),
        WsMessage::Close(_) => Ok(FrameAction::Close),
        WsMessage::Text(_) => Err(websocket_invalid_data_error(
            "websocket transport requires binary messages",
        )),
    }
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        path.trim_end_matches('/').to_string()
    } else {
        format!("/{}", path.trim_end_matches('/'))
    }
}

fn path_not_found_response(request: &Request) -> ErrorResponse {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Some(format!(
            "REPE websocket endpoint not found: {}",
            request.uri().path()
        )))
        .expect("valid handshake rejection response")
}

fn websocket_transport_error(err: tungstenite::Error) -> RepeError {
    match err {
        tungstenite::Error::Io(io_err) => RepeError::Io(io_err),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => RepeError::Io(
            std::io::Error::new(ErrorKind::ConnectionAborted, "websocket connection closed"),
        ),
        other => RepeError::Io(std::io::Error::other(other.to_string())),
    }
}

fn websocket_invalid_data_error(message: &str) -> RepeError {
    RepeError::Io(std::io::Error::new(ErrorKind::InvalidData, message))
}

struct WebSocketPathValidator {
    expected: String,
}

impl Callback for WebSocketPathValidator {
    #[allow(clippy::result_large_err)]
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        if request.uri().path() == self.expected {
            Ok(response)
        } else {
            Err(path_not_found_response(request))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket_client::WebSocketClient;
    use serde_json::json;

    #[tokio::test(flavor = "current_thread")]
    async fn websocket_server_roundtrip() {
        let router = Router::new().with_json("/mul", |value| {
            let a = value.get("a").and_then(|v| v.as_i64()).unwrap_or_default();
            let b = value.get("b").and_then(|v| v.as_i64()).unwrap_or_default();
            Ok(json!({ "prod": a * b }))
        });

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws_stream = accept_repe_websocket(stream, "/repe").await.unwrap();
            handle_connection(ws_stream, router).await.unwrap();
        });

        let client = WebSocketClient::connect(&format!("ws://{addr}/repe"))
            .await
            .unwrap();
        let out = client
            .call_json("/mul", &json!({ "a": 6, "b": 7 }))
            .await
            .unwrap();
        assert_eq!(out["prod"], 42);

        drop(client);
        server_task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn websocket_proxy_forwards_raw_messages() {
        let upstream_router = Router::new().with_json("/echo", Ok);
        let upstream_listener = crate::async_server::AsyncServer::listen(("127.0.0.1", 0))
            .await
            .unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            crate::async_server::AsyncServer::new(upstream_router)
                .serve(upstream_listener)
                .await
                .unwrap();
        });

        let proxy_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            let upstream = AsyncClient::connect(upstream_addr).await.unwrap();
            let (stream, _) = proxy_listener.accept().await.unwrap();
            let ws_stream = accept_repe_websocket(stream, "/repe").await.unwrap();
            proxy_connection(ws_stream, upstream).await.unwrap();
        });

        let client = WebSocketClient::connect(&format!("ws://{proxy_addr}/repe"))
            .await
            .unwrap();
        let out = client
            .call_json("/echo", &json!({ "ok": true }))
            .await
            .unwrap();
        assert_eq!(out, json!({ "ok": true }));

        drop(client);
        proxy_task.await.unwrap();
        upstream_task.abort();
    }
}
