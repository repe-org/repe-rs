//! A JSON REPE client over TCP. Pair with `examples/server.rs`.
//!
//! Each call names the type it sends and the type it expects back. There is no
//! untyped call: with no document model, a response has to be decoded into
//! something, and naming that something is what turns a wrong answer into a
//! decode error instead of a `None` discovered later.

use repe::Client;

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect("127.0.0.1:8081")?;

    let pong: Pong = client.call_typed_json("/ping", &Empty)?;
    println!("/ping => pong={}", pong.pong);

    let echo: Message = client.call_typed_json(
        "/echo",
        &Message {
            msg: "hello".into(),
        },
    )?;
    println!("/echo => {}", echo.msg);

    let status: Status = client.call_typed_json("/status", &Empty)?;
    println!(
        "/status => {}, up {:.1}s",
        status.status, status.uptime_seconds
    );

    let add: AddResp = client.call_typed_json("/add", &AddReq { a: 4, b: 5 })?;
    println!("/add => {}", add.sum);

    Ok(())
}
