//! Scripted REPE WebSocket server for the `WasmClient` browser tests.
//!
//! Deliberately not a [`repe::WebSocketServer`]. The interesting cases are ones
//! a well-behaved server never produces -- a notify whose id collides with an
//! in-flight request, a frame that will not decode -- and the peer API has no
//! way to express them. This speaks the wire directly instead.
//!
//! Every route reacts to a request the client sent. Nothing here is timed, so
//! the orderings the tests assert are the ones the WebSocket protocol already
//! guarantees, and there is nothing to flake.
//!
//! See `wasm-tests/README.md`.

use futures_util::{SinkExt, StreamExt};
use repe::constants::QueryFormat;
use repe::message::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Kept in step with `SERVER_URL` in `wasm-tests/client/tests/browser.rs`.
const DEFAULT_PORT: u16 = 8791;

type Ws = WebSocketStream<TcpStream>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = match std::env::var("REPE_WASM_TEST_PORT") {
        Ok(raw) => raw.parse()?,
        Err(_) => DEFAULT_PORT,
    };

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    // The test harness waits for this line before launching the browser.
    println!("repe wasm test server listening on 127.0.0.1:{port}");

    loop {
        let (stream, _) = listener.accept().await?;
        // One task per connection: browser test runners open several at once,
        // and a scenario that closes its socket must not disturb the others.
        tokio::spawn(async move {
            if let Err(err) = serve(stream).await {
                eprintln!("connection ended: {err}");
            }
        });
    }
}

async fn serve(stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let mut ws = tokio_tungstenite::accept_async(stream).await?;

    while let Some(frame) = ws.next().await {
        let payload = match frame? {
            WsMessage::Binary(payload) => payload,
            WsMessage::Close(_) => break,
            // Ping/Pong are handled by tungstenite; anything else is not ours.
            _ => continue,
        };

        let request = Message::from_slice_exact(&payload)?;
        if dispatch(&mut ws, &request).await? == Flow::Close {
            break;
        }
    }

    Ok(())
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Close,
}

async fn dispatch(ws: &mut Ws, request: &Message) -> Result<Flow, Box<dyn std::error::Error>> {
    let id = request.header.id;

    match request.query_str()? {
        // Baseline: proves the request/response correlation path works at all,
        // so a failure in any other test is about that test's subject.
        "/echo" => {
            send(ws, response(id, "/echo", request.json_body()?)).await?;
        }

        // The notify precedes the response on the wire, so by the time the
        // client's `call` resolves, the notify has already been routed.
        "/notify-then-respond" => {
            send(ws, notify(0, "/pushed", serde_json::json!({ "seq": 1 }))).await?;
            send(ws, response(id, "/ack", serde_json::json!({ "ok": true }))).await?;
        }

        // The case the notify-before-correlation rule exists for: a notify
        // carrying the *same* id as the request in flight. A client that checks
        // its pending map first resolves the request with this notify and then
        // has no waiter left for the real response.
        "/collide" => {
            send(ws, notify(id, "/collided", serde_json::json!({ "seq": 1 }))).await?;
            send(ws, response(id, "/ack", serde_json::json!({ "ok": true }))).await?;
        }

        // Respond first so the client has no request in flight, then send a
        // frame that cannot decode, then a valid notify. A client that treats a
        // decode failure as connection death drops the subscription and never
        // delivers the notify -- on a socket that is still perfectly alive.
        "/undecodable-then-notify" => {
            send(ws, response(id, "/ack", serde_json::json!({ "ok": true }))).await?;
            ws.send(WsMessage::Text("not a repe frame".into())).await?;
            send(
                ws,
                notify(0, "/after-undecodable", serde_json::json!({ "seq": 1 })),
            )
            .await?;
        }

        // Server-initiated close with the client still alive, which is the only
        // way a push-only consumer can learn the connection is gone.
        "/close-after-response" => {
            send(ws, response(id, "/ack", serde_json::json!({ "ok": true }))).await?;
            ws.close(None).await?;
            return Ok(Flow::Close);
        }

        unknown => {
            send(
                ws,
                response(
                    id,
                    "/error",
                    serde_json::json!({ "unknown_route": unknown }),
                ),
            )
            .await?;
        }
    }

    Ok(Flow::Continue)
}

async fn send(ws: &mut Ws, message: Message) -> Result<(), Box<dyn std::error::Error>> {
    ws.send(WsMessage::Binary(message.to_vec())).await?;
    Ok(())
}

fn response(id: u64, query: &str, body: serde_json::Value) -> Message {
    build(id, query, body, false)
}

fn notify(id: u64, query: &str, body: serde_json::Value) -> Message {
    build(id, query, body, true)
}

fn build(id: u64, query: &str, body: serde_json::Value, notify: bool) -> Message {
    Message::builder()
        .id(id)
        .notify(notify)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .body_json(&body)
        .expect("fixture bodies are valid JSON")
        .build()
}
