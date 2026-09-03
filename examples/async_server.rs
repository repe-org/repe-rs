use repe::{AsyncServer, Router};
use std::time::Instant;

/// An empty body: `{}` on the wire. A route that takes no arguments still names
/// a type, and this is the one for it.
#[derive(Default, Debug)]
struct Empty;
structio::object!(Empty {});

#[derive(Default, Debug)]
struct Pong {
    pong: bool,
}
structio::object!(Pong { pong });

#[derive(Default, Debug)]
struct Status {
    status: String,
    uptime_seconds: f64,
}
structio::object!(Status {
    status,
    uptime_seconds
});

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
        .with_typed("/ping", |_: Empty| Ok(Pong { pong: true }))
        .with_typed("/mul", |r: MulReq| Ok(MulResp { product: r.x * r.y }))
        .with_typed("/add", |r: AddReq| Ok(AddResp { sum: r.a + r.b }))
        .with_typed("/status", move |_: Empty| {
            Ok(Status {
                status: "ok".into(),
                uptime_seconds: started.elapsed().as_secs_f64(),
            })
        });

    let server = AsyncServer::new(router);
    let listener = AsyncServer::listen("127.0.0.1:8082").await?;
    eprintln!("Async REPE JSON server listening on 127.0.0.1:8082");
    server.serve(listener).await
}
