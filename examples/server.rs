use repe::{Router, Server};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let started = Instant::now();
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
        .with("/echo", |v: Value| Ok(json!({"echo": v})))
        .with("/status", move |_v: Value| {
            let uptime = started.elapsed().as_secs_f64();
            Ok(json!({
                "status": "ok",
                "uptime_seconds": uptime
            }))
        })
        .with_typed("/add", |r: AddReq| Ok(AddResp { sum: r.a + r.b }));

    let server = Server::new(router)
        .read_timeout(Some(Duration::from_secs(120)))
        .write_timeout(Some(Duration::from_secs(120)));

    let listener = server.listen("127.0.0.1:8081")?;
    eprintln!("REPE JSON server listening on 127.0.0.1:8081");
    server.serve(listener)
}
