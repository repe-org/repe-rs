//! A JSON REPE server over TCP.
//!
//! Every route names the type it takes and the type it returns, and each is
//! declared once with `structio::object!`. That declaration is the encoding:
//! there is no derive, no attribute, and no intermediate document — the router
//! reads the request body directly into the parameter and writes the return
//! value straight into the response frame.
//!
//! Pair with `examples/client.rs`.

use repe::{Router, Server};
use std::time::{Duration, Instant};

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
struct Message {
    msg: String,
}
structio::object!(Message { msg });

#[derive(Default, Debug)]
struct Status {
    status: String,
    uptime_seconds: f64,
}
structio::object!(Status {
    status,
    uptime_seconds
});

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

fn main() -> std::io::Result<()> {
    let started = Instant::now();

    let router = Router::new()
        .with_typed("/ping", |_: Empty| Ok(Pong { pong: true }))
        // Echo is a type in and the same type out, which is what an echo is.
        .with_typed("/echo", |v: Message| Ok(v))
        .with_typed("/status", move |_: Empty| {
            Ok(Status {
                status: "ok".into(),
                uptime_seconds: started.elapsed().as_secs_f64(),
            })
        })
        .with_typed("/add", |r: AddReq| Ok(AddResp { sum: r.a + r.b }));

    let server = Server::new(router)
        .read_timeout(Some(Duration::from_secs(120)))
        .write_timeout(Some(Duration::from_secs(120)));

    let listener = server.listen("127.0.0.1:8081")?;
    eprintln!("REPE JSON server listening on 127.0.0.1:8081");
    server.serve(listener)
}
