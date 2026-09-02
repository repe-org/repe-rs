use repe::{AsyncServer, Router};
use std::time::Instant;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let started = Instant::now();

    #[derive(Default, Debug)]
    struct MulReq {
        x: i64,
        y: i64,
    }
    structio::object!(MulReq { x, y });
    #[derive(Default, Debug)]
    struct MulResp {
        product: i64,
    }
    structio::object!(MulResp { product });
    #[derive(Default, Debug)]
    struct AddReq {
        a: i64,
        b: i64,
    }
    structio::object!(AddReq { a, b });
    #[derive(Default, Debug)]
    struct AddResp {
        sum: i64,
    }
    structio::object!(AddResp { sum });

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
