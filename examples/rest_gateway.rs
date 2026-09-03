//! A REST facade in front of a REPE core, both serving the same `Registry`.
//!
//! Run with:
//!
//! ```text
//! cargo run --features rest --example rest_gateway
//! ```
//!
//! Then, against the REST leg:
//!
//! ```text
//! curl -i localhost:8080/api/v1/counter
//! curl -i -X PUT -d 42 localhost:8080/api/v1/counter
//! curl -i -X POST -d '{"a":2,"b":3}' localhost:8080/api/v1/add
//! curl -i -H 'If-None-Match: "..."' localhost:8080/api/v1/counter   # 304
//! curl -i -X PUT -d 1 localhost:8080/api/v1/counter                 # same as POST
//! curl -i -X OPTIONS localhost:8080/api/v1/add                      # Allow
//! ```
//!
//! and against the REPE leg, reaching the same state:
//!
//! ```text
//! cargo run --features cli -- --url 127.0.0.1:8081 get /api/v1/counter
//! ```

use repe::rest::RestGateway;
use repe::structs::RequestBody;
use repe::{AsyncServer, ErrorCode, Registry, Router};
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
    name: String,
    verbose: bool,
}
structio::object!(Config { name, verbose });

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());
    // Every registered path is a function, so a stateful endpoint is one that
    // owns its state: a body sets the counter, no body reads it.
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
            name: "demo".into(),
            verbose: false,
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

    // One registry, two front doors. The REPE server keeps the binary fast path
    // for clients that need it; the gateway adds curl, OpenAPI, and edge caching
    // for everyone else. Neither is a translation of the other — they are two
    // carriers over the same state.
    let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
    let repe = AsyncServer::new(router);
    let repe_listener = AsyncServer::listen("127.0.0.1:8081").await?;
    println!("REPE  on 127.0.0.1:8081");
    tokio::spawn(async move {
        let _ = repe.serve(repe_listener).await;
    });

    let gateway = RestGateway::new("/api/v1", registry);
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    println!("REST  on http://127.0.0.1:8080/api/v1");
    gateway.serve(http_listener).await?;
    Ok(())
}
