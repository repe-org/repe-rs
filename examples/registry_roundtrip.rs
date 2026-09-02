//! Driving a registry-backed router in process, with no socket.
//!
//! Every registered path is a function, and a call is one shape: the body is
//! the arguments, the return value is the response. The frame header picks the
//! body format per request, so the same function answers a JSON call and a BEVE
//! one without a transcode step in between.
//!
//! Run with: `cargo run --example registry_roundtrip`

use repe::structs::RequestBody;
use repe::{BodyFormat, ErrorCode, Message, QueryFormat, Registry, Router};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Default, Debug)]
struct Operands {
    a: i64,
    b: i64,
}
structio::object!(Operands { a, b });

fn request_read(path: &str) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .build()
}

fn request_json<T: structio::json::Write + ?Sized>(path: &str, body: &T) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(body)
        .build()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());

    let counter = Arc::new(AtomicI64::new(0));
    registry.register_function("/counter", move |params: Option<RequestBody<'_>>| {
        if let Some(body) = params {
            let next: i64 = body
                .read("/counter")
                .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
            counter.store(next, Ordering::SeqCst);
        }
        Ok(counter.load(Ordering::SeqCst))
    })?;

    registry.register_function("/add", |params: Option<RequestBody<'_>>| {
        let Some(body) = params else {
            return Err((ErrorCode::InvalidBody, "expected an object body".into()));
        };
        let operands: Operands = body
            .read("/add")
            .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
        Ok(operands.a + operands.b)
    })?;

    let router = Router::new().with_registry("", Arc::clone(&registry));

    let call = |request: &Message| -> Result<Message, Box<dyn std::error::Error>> {
        let path = request.query_str()?;
        Ok(router
            .get(path)
            .unwrap_or_else(|| panic!("no handler for {path}"))
            .handle(request)?)
    };

    let read = call(&request_read("/counter"))?;
    println!("read /counter  => {}", read.body_utf8());

    let write = call(&request_json("/counter", &42i64))?;
    println!("set  /counter  => {}", write.body_utf8());

    let after = call(&request_read("/counter"))?;
    println!("read /counter  => {}", after.body_utf8());

    let sum = call(&request_json("/add", &Operands { a: 2, b: 3 }))?;
    println!("call /add      => {}", sum.body_utf8());

    // The same function over BEVE. The header declares the format on each leg,
    // so a BEVE request comes back in BEVE.
    let beve_request = Message::builder()
        .id(1)
        .query_str("/add")
        .query_format(QueryFormat::JsonPointer)
        .body_beve(&Operands { a: 20, b: 22 })
        .build();
    let beve_sum = call(&beve_request)?;
    assert_eq!(beve_sum.header.body_format, BodyFormat::Beve as u16);
    println!("call /add BEVE => {}", beve_sum.decode_body::<i64>()?);

    Ok(())
}
