//! Graceful-shutdown WebSocket server demo.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example websocket_graceful_server --features websocket
//! ```
//!
//! Demonstrates three post-2.6 operability features working together:
//!
//! * an off-reader (`_blocking`) handler that polls
//!   [`CallContext::is_cancelled`](repe::CallContext::is_cancelled) at loop
//!   boundaries and winds down promptly instead of running to completion,
//! * an [`on_error`](repe::WebSocketServer::on_error) hook that routes
//!   transport-level events into your own logging instead of stderr, and
//! * [`serve_listener_with_graceful_drain`](repe::WebSocketServer::serve_listener_with_graceful_drain),
//!   which on shutdown stops accepting, cancels in-flight handlers, and
//!   awaits connections up to a deadline before aborting the rest.
//!
//! The program runs a server and a client in one process: the client
//! starts the long handler, then we trigger shutdown and watch the handler
//! wind down and the server return cleanly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use repe::server::Router;
use repe::{ConnectionError, WebSocketClient, WebSocketServer};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A long off-reader handler that pretends to stream work, checking its
    // cancellation signal at each step. Without the check it would hold its
    // `spawn_blocking` thread until the work finished; with it, a shutdown
    // (or a client disconnect) winds it down at the next loop boundary.
    let wound_down = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&wound_down);
    let router = Router::new().with_json_ctx_blocking("/work", move |ctx, _params| {
        let mut step = 0u64;
        loop {
            if ctx.is_cancelled() {
                println!("[handler] cancelled at step {step}; freeing the thread");
                flag.store(true, Ordering::SeqCst);
                return Ok(json!({ "cancelled_at": step }));
            }
            // A real handler would produce a chunk here and push it to the
            // caller via `ctx.peer()`.
            step += 1;
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = WebSocketServer::new(router).on_error(|err: &ConnectionError| {
        // Route transport-level events wherever your application logs.
        // Returning here without printing would suppress the default stderr
        // output entirely.
        println!("[on_error] {err}");
    });

    let serve = tokio::spawn(async move {
        server
            .serve_listener_with_graceful_drain(
                listener,
                "/repe",
                async {
                    let _ = shutdown_rx.await;
                },
                Duration::from_secs(5),
            )
            .await
    });

    // Connect and kick off the long handler as a notify, so we don't block
    // waiting for a response (which only arrives once the handler is
    // cancelled).
    let client = WebSocketClient::connect(&format!("ws://{addr}/repe")).await?;
    client.notify_json("/work", &json!({})).await?;
    println!("[client] started /work; letting it run briefly...");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger the graceful drain: stop accepting, cancel in-flight handlers,
    // and await connections for up to the 5 s deadline before aborting.
    println!("[main] triggering graceful shutdown");
    let _ = shutdown_tx.send(());

    let result = serve.await?;
    println!("[main] serve returned: {result:?}");

    // The handler observes the cancel within its 50 ms poll interval, which
    // can land just after the drain (and thus `serve`) returns -- so give it
    // a moment.
    for _ in 0..80 {
        if wound_down.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        wound_down.load(Ordering::SeqCst),
        "handler did not observe cancellation"
    );
    println!("[main] handler wound down cleanly on shutdown");

    drop(client);
    Ok(())
}
