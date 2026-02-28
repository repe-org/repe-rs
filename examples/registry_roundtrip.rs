use repe::{BodyFormat, ErrorCode, Message, QueryFormat, Registry, Router};
use serde_json::{Value, json};
use std::sync::Arc;

fn request_read(path: &str) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .build()
}

fn request_json(path: &str, body: &Value) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(body)
        .expect("json body")
        .build()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());
    registry.register_value("/counter", json!(0))?;
    registry.register_function("/add", |params| {
        let Some(Value::Object(map)) = params else {
            return Err((ErrorCode::InvalidBody, "expected object body".into()));
        };
        let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
        let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
        Ok(json!(a + b))
    })?;

    let router = Router::new().with_registry("", Arc::clone(&registry));

    let read_counter = router
        .get("/counter")
        .expect("counter handler")
        .handle(&request_read("/counter"))?;
    println!("read /counter => {}", read_counter.body_utf8());

    let write_counter = router
        .get("/counter")
        .expect("counter handler")
        .handle(&request_json("/counter", &json!(42)))?;
    println!("write /counter => {}", write_counter.body_utf8());

    let call_add = router
        .get("/add")
        .expect("add handler")
        .handle(&request_json("/add", &json!({"a": 2, "b": 3})))?;
    println!("call /add => {}", call_add.body_utf8());

    // Registry bodies can be encoded with BEVE as well.
    let beve_request = Message::builder()
        .id(1)
        .query_str("/counter")
        .query_format(QueryFormat::JsonPointer)
        .body_beve(&json!(7))?
        .build();
    let beve_write = router
        .get("/counter")
        .expect("counter handler")
        .handle(&beve_request)?;
    assert_eq!(beve_write.header.body_format, BodyFormat::Json as u16);

    Ok(())
}
