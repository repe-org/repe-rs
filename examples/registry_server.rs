use repe::{ErrorCode, Registry, Router, Server};
use serde_json::{Value, json};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());

    registry.register_value("/counter", json!(0))?;
    registry.register_value("/config", json!({"timeout": 30, "retries": 3}))?;
    registry.register_function("/add", |params| {
        let Some(Value::Object(map)) = params else {
            return Err((ErrorCode::InvalidBody, "expected object body".into()));
        };
        let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
        let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
        Ok(json!({"result": a + b}))
    })?;

    let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
    let server = Server::new(router);
    let listener = server.listen("127.0.0.1:8082")?;

    eprintln!("REPE registry server listening on 127.0.0.1:8082");
    eprintln!("  read:  /api/v1/counter");
    eprintln!("  write: /api/v1/counter (JSON body)");
    eprintln!("  call:  /api/v1/add (JSON body {{\"a\":1,\"b\":2}})");

    server.serve(listener)?;
    Ok(())
}
