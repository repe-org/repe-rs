#![cfg(not(target_arch = "wasm32"))]

use repe::{BodyFormat, ErrorCode, Message, QueryFormat, Registry, Router};
use serde_json::{Value, json};
use std::sync::Arc;
use std::thread;

fn request_empty(path: &str) -> Message {
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

fn request_utf8(path: &str, body: &str) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_utf8(body)
        .build()
}

fn request_beve(path: &str, body: &Value) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_beve(body)
        .expect("beve body")
        .build()
}

fn request_raw(path: &str, body: &[u8]) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(body.to_vec())
        .body_format(BodyFormat::RawBinary)
        .build()
}

#[test]
fn registry_router_read_write_call_semantics() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/counter", Value::from(0))
        .expect("register value");
    registry
        .register_function("/add", |params| {
            let Some(Value::Object(map)) = params else {
                return Err((ErrorCode::InvalidBody, "expected object body".into()));
            };
            let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
            let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
            Ok(Value::from(a + b))
        })
        .expect("register function");

    let router = Router::new().with_registry("", Arc::clone(&registry));

    let read = router
        .get("/counter")
        .expect("counter handler")
        .handle(&request_empty("/counter"))
        .expect("read response");
    assert_eq!(read.header.ec, ErrorCode::Ok as u32);
    assert_eq!(read.json_body::<Value>().expect("json body"), json!(0));

    let write = router
        .get("/counter")
        .expect("counter handler")
        .handle(&request_json("/counter", &json!(42)))
        .expect("write response");
    assert_eq!(write.header.ec, ErrorCode::Ok as u32);
    assert_eq!(
        write.json_body::<Value>().expect("json body"),
        json!({"status":"ok","path":"/counter"})
    );

    let read_after = router
        .get("/counter")
        .expect("counter handler")
        .handle(&request_empty("/counter"))
        .expect("read response");
    assert_eq!(
        read_after.json_body::<Value>().expect("json body"),
        json!(42)
    );

    let function_info = router
        .get("/add")
        .expect("add handler")
        .handle(&request_empty("/add"))
        .expect("function info response");
    assert_eq!(
        function_info.json_body::<Value>().expect("json body"),
        json!({"type":"function","path":"/add"})
    );

    let call = router
        .get("/add")
        .expect("add handler")
        .handle(&request_json("/add", &json!({"a": 2, "b": 3})))
        .expect("call response");
    assert_eq!(call.header.ec, ErrorCode::Ok as u32);
    assert_eq!(call.json_body::<Value>().expect("json body"), json!(5));
}

#[test]
fn registry_path_prefix_routes_only_matching_paths() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/status", json!("ok"))
        .expect("register value");

    let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
    assert!(router.get("/status").is_none());

    let response = router
        .get("/api/v1/status")
        .expect("prefixed handler")
        .handle(&request_empty("/api/v1/status"))
        .expect("response");
    assert_eq!(response.header.ec, ErrorCode::Ok as u32);
    assert_eq!(
        response.json_body::<Value>().expect("json body"),
        json!("ok")
    );
}

#[test]
fn registry_utf8_body_writes_plain_string_value() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/name", json!("initial"))
        .expect("register value");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let write = router
        .get("/name")
        .expect("name handler")
        .handle(&request_utf8("/name", "alice"))
        .expect("write response");
    assert_eq!(write.header.ec, ErrorCode::Ok as u32);

    let read = router
        .get("/name")
        .expect("name handler")
        .handle(&request_empty("/name"))
        .expect("read response");
    assert_eq!(
        read.json_body::<Value>().expect("json body"),
        json!("alice")
    );
}

#[test]
fn registry_root_write_requires_object_body() {
    let registry = Arc::new(Registry::new());
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = router
        .get("/")
        .expect("root handler")
        .handle(&request_json("/", &json!(7)))
        .expect("response");
    assert_eq!(response.header.ec, ErrorCode::InvalidBody as u32);
    assert!(
        response
            .error_message_utf8()
            .expect("error message")
            .contains("root write requires")
    );
}

#[test]
fn registry_beve_body_writes_object_value() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/payload", Value::Null)
        .expect("register placeholder");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = router
        .get("/payload")
        .expect("handler")
        .handle(&request_beve("/payload", &json!({"n": 7, "ok": true})))
        .expect("write response");
    assert_eq!(response.header.ec, ErrorCode::Ok as u32);

    let read = router
        .get("/payload")
        .expect("handler")
        .handle(&request_empty("/payload"))
        .expect("read response");
    assert_eq!(
        read.json_body::<Value>().expect("json body"),
        json!({"n": 7, "ok": true})
    );
}

#[test]
fn registry_raw_body_writes_json_array_of_bytes() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/blob", Value::Null)
        .expect("register placeholder");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = router
        .get("/blob")
        .expect("handler")
        .handle(&request_raw("/blob", &[1, 2, 255]))
        .expect("write response");
    assert_eq!(response.header.ec, ErrorCode::Ok as u32);

    let read = router
        .get("/blob")
        .expect("handler")
        .handle(&request_empty("/blob"))
        .expect("read response");
    assert_eq!(
        read.json_body::<Value>().expect("json body"),
        json!([1, 2, 255])
    );
}

#[test]
fn registry_unknown_body_format_is_invalid_body() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/blob", Value::Null)
        .expect("register placeholder");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let mut request = Message::builder()
        .id(1)
        .query_str("/blob")
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(vec![1, 2, 3])
        .build();
    request.header.body_format = 7777;

    let response = router
        .get("/blob")
        .expect("handler")
        .handle(&request)
        .expect("response");
    assert_eq!(response.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn registry_invalid_utf8_body_is_invalid_body() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/name", json!("init"))
        .expect("register placeholder");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let request = Message::builder()
        .id(1)
        .query_str("/name")
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(vec![0xFF, 0xFE])
        .body_format(BodyFormat::Utf8)
        .build();

    let response = router
        .get("/name")
        .expect("handler")
        .handle(&request)
        .expect("response");
    assert_eq!(response.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn registry_array_index_read_write_and_error_cases() {
    let registry = Arc::new(Registry::new());
    registry.set_root(json!({"items":[10,20,30]}));
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let read = router
        .get("/items/1")
        .expect("handler")
        .handle(&request_empty("/items/1"))
        .expect("response");
    assert_eq!(read.json_body::<Value>().expect("json body"), json!(20));

    let write = router
        .get("/items/1")
        .expect("handler")
        .handle(&request_json("/items/1", &json!(99)))
        .expect("response");
    assert_eq!(write.header.ec, ErrorCode::Ok as u32);

    let read_after = router
        .get("/items/1")
        .expect("handler")
        .handle(&request_empty("/items/1"))
        .expect("response");
    assert_eq!(
        read_after.json_body::<Value>().expect("json body"),
        json!(99)
    );

    let invalid_index = router
        .get("/items/nope")
        .expect("handler")
        .handle(&request_empty("/items/nope"))
        .expect("response");
    assert_eq!(invalid_index.header.ec, ErrorCode::MethodNotFound as u32);

    let out_of_bounds = router
        .get("/items/99")
        .expect("handler")
        .handle(&request_empty("/items/99"))
        .expect("response");
    assert_eq!(out_of_bounds.header.ec, ErrorCode::MethodNotFound as u32);
}

#[test]
fn registry_escaped_pointer_paths_roundtrip() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/a~1b", json!(123))
        .expect("register escaped path");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let read = router
        .get("/a~1b")
        .expect("handler")
        .handle(&request_empty("/a~1b"))
        .expect("response");
    assert_eq!(read.json_body::<Value>().expect("json body"), json!(123));

    let write = router
        .get("/a~1b")
        .expect("handler")
        .handle(&request_json("/a~1b", &json!(456)))
        .expect("response");
    assert_eq!(write.header.ec, ErrorCode::Ok as u32);

    let read_after = router
        .get("/a~1b")
        .expect("handler")
        .handle(&request_empty("/a~1b"))
        .expect("response");
    assert_eq!(
        read_after.json_body::<Value>().expect("json body"),
        json!(456)
    );
}

#[test]
fn registry_function_error_code_propagates() {
    let registry = Arc::new(Registry::new());
    registry
        .register_function("/boom", |_params| {
            Err((ErrorCode::ApplicationErrorBase, "denied".into()))
        })
        .expect("register function");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = router
        .get("/boom")
        .expect("handler")
        .handle(&request_json("/boom", &json!({"x": 1})))
        .expect("response");
    assert_eq!(response.header.ec, ErrorCode::ApplicationErrorBase as u32);
    assert_eq!(response.error_message_utf8().as_deref(), Some("denied"));
}

#[test]
fn registry_merge_helpers_update_nested_objects() {
    let registry = Registry::new();
    registry
        .merge_root(serde_json::Map::from_iter([
            ("status".to_string(), json!("ok")),
            ("version".to_string(), json!("1.0")),
        ]))
        .expect("merge root");
    registry
        .register_value("/api", json!({}))
        .expect("register api object");
    registry
        .merge_at(
            "/api",
            serde_json::Map::from_iter([
                ("users".to_string(), json!(3)),
                ("posts".to_string(), json!(5)),
            ]),
        )
        .expect("merge at path");

    assert_eq!(registry.read_value("/status").expect("status"), json!("ok"));
    assert_eq!(
        registry.read_value("/version").expect("version"),
        json!("1.0")
    );
    assert_eq!(registry.read_value("/api/users").expect("users"), json!(3));
    assert_eq!(registry.read_value("/api/posts").expect("posts"), json!(5));
}

#[test]
fn registry_prefix_exact_and_trailing_slash_hit_root() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/status", json!("ok"))
        .expect("register status");
    let router = Router::new().with_registry("/api/v1/", Arc::clone(&registry));

    let exact = router
        .get("/api/v1")
        .expect("exact prefix handler")
        .handle(&request_empty("/api/v1"))
        .expect("response");
    assert_eq!(exact.header.ec, ErrorCode::Ok as u32);
    assert_eq!(
        exact.json_body::<Value>().expect("json body"),
        json!({"status":"ok"})
    );

    let slash = router
        .get("/api/v1/")
        .expect("slash prefix handler")
        .handle(&request_empty("/api/v1/"))
        .expect("response");
    assert_eq!(slash.header.ec, ErrorCode::Ok as u32);
    assert_eq!(
        slash.json_body::<Value>().expect("json body"),
        json!({"status":"ok"})
    );
}

#[test]
fn registry_invalid_pointer_error_code_maps_to_method_not_found() {
    let registry = Registry::new();
    let err = registry
        .dispatch("not/a/pointer", None)
        .expect_err("invalid pointer should fail");
    assert_eq!(err.code(), ErrorCode::MethodNotFound);
}

#[test]
fn registry_concurrent_dispatch_stress() {
    let registry = Arc::new(Registry::new());
    registry
        .register_value("/counter", json!(0))
        .expect("register counter");

    let workers = (0..8)
        .map(|worker| {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                for i in 0..200 {
                    let value = (worker * 1_000 + i) as i64;
                    registry
                        .dispatch("/counter", Some(json!(value)))
                        .expect("write should succeed");
                    let _ = registry
                        .dispatch("/counter", None)
                        .expect("read should succeed");
                }
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        worker.join().expect("worker should not panic");
    }

    let final_value = registry.dispatch("/counter", None).expect("final read");
    assert!(final_value.as_i64().is_some());
}
