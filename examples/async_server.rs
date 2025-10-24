use repe::{AsyncServer, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Instant;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let started = Instant::now();

    #[derive(Debug, Deserialize)]
    struct MulReq {
        x: i64,
        y: i64,
    }
    #[derive(Debug, Serialize)]
    struct MulResp {
        product: i64,
    }
    #[derive(Debug, Deserialize)]
    struct AddReq {
        a: i64,
        b: i64,
    }
    #[derive(Debug, Serialize)]
    struct AddResp {
        sum: i64,
    }

    let router = Router::new()
        .with("/ping", |_v: Value| Ok(json!({"pong": true})))
        .with_typed("/mul", |r: MulReq| Ok(MulResp { product: r.x * r.y }))
        .with_typed("/add", |r: AddReq| Ok(AddResp { sum: r.a + r.b }))
        .with("/status", move |_v: Value| {
            Ok(json!({ "status": "ok", "uptime_seconds": started.elapsed().as_secs_f64() }))
        });

    let server = AsyncServer::new(router);
    let listener = AsyncServer::listen("127.0.0.1:8082").await?;
    eprintln!("Async REPE JSON server listening on 127.0.0.1:8082");
    server.serve(listener).await
}
