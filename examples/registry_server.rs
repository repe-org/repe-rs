//! A registry-backed REPE server.
//!
//! The [`Registry`] is a table of functions resolved by JSON Pointer. It used
//! to be a document store as well — a pointer could name a stored value, and a
//! body against one meant "assign" — and that went with the document model. So
//! a stateful endpoint is a function that owns its state and decides what a
//! body means, which is what `/counter` is here.
//!
//! Run with: `cargo run --example registry_server`

use repe::structs::RequestBody;
use repe::{ErrorCode, Registry, Router, Server};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Default, Debug)]
struct Operands {
    a: i64,
    b: i64,
}
structio::object!(Operands { a, b });

#[derive(Default, Debug)]
struct Sum {
    result: i64,
}
structio::object!(Sum { result });

#[derive(Default, Debug)]
struct Config {
    timeout: i64,
    retries: i64,
}
structio::object!(Config { timeout, retries });

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());

    // A counter behind a function: a body sets it, no body reads it. The rule
    // is the handler's to make, which is the point — the registry no longer
    // decides what a body means on the handler's behalf.
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

    registry.register_function("/config", |_: Option<RequestBody<'_>>| {
        Ok(Config {
            timeout: 30,
            retries: 3,
        })
    })?;

    registry.register_function("/add", |params: Option<RequestBody<'_>>| {
        let Some(body) = params else {
            return Err((ErrorCode::InvalidBody, "expected an object body".into()));
        };
        let operands: Operands = body
            .read("/add")
            .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
        Ok(Sum {
            result: operands.a + operands.b,
        })
    })?;

    let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
    let server = Server::new(router);
    let listener = server.listen("127.0.0.1:8082")?;

    eprintln!("REPE registry server listening on 127.0.0.1:8082");
    eprintln!("  read:  /api/v1/counter        (no body)");
    eprintln!("  set:   /api/v1/counter        (JSON body: 42)");
    eprintln!("  call:  /api/v1/add            (JSON body: {{\"a\":1,\"b\":2}})");

    server.serve(listener)?;
    Ok(())
}
