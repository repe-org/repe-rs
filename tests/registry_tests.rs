//! The [`Registry`] is a table of functions resolved by JSON Pointer.
//!
//! It used to be a document store as well: a pointer could name a stored
//! `serde_json::Value`, and a body against one meant "assign". That went with
//! the value tree, and with it went reads, writes, merges, array indexing, and
//! the read-or-write-or-call decision made from the body's shape. What is left
//! is one thing done one way: a pointer names a function, a body is its
//! arguments, and what it writes is the response.

#![cfg(not(target_arch = "wasm32"))]

use repe::structs::{RequestBody, ResponseBody};
use repe::{BodyFormat, ErrorCode, Message, QueryFormat, Registry, Router};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;

#[derive(Default, Debug, PartialEq)]
struct Operands {
    a: i64,
    b: i64,
}
structio::object!(Operands { a, b });

fn request_empty(path: &str) -> Message {
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

fn request_beve<T: structio::beve::Write + ?Sized>(path: &str, body: &T) -> Message {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_beve(body)
        .build()
}

/// Dispatch `request` through the router and return the response, failing the
/// test if no route claims the path.
fn serve(router: &Router, path: &str, request: &Message) -> Message {
    router
        .get(path)
        .unwrap_or_else(|| panic!("no handler for {path}"))
        .handle(request)
        .expect("a non-notify request produces a response")
}

fn adder() -> Arc<Registry> {
    let registry = Arc::new(Registry::new());
    registry
        .register_function("/add", |params: Option<RequestBody<'_>>| {
            let Some(body) = params else {
                return Err((ErrorCode::InvalidBody, "expected an object body".into()));
            };
            let operands: Operands = body
                .read("/add")
                .map_err(|err| (ErrorCode::InvalidBody, err.to_string()))?;
            Ok(operands.a + operands.b)
        })
        .expect("register function");
    registry
}

#[test]
fn a_call_carries_its_arguments_in_the_body() {
    let registry = adder();
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = serve(
        &router,
        "/add",
        &request_json("/add", &Operands { a: 2, b: 3 }),
    );
    assert_eq!(response.header.ec, ErrorCode::Ok as u32);
    assert_eq!(response.json_body::<i64>().expect("json body"), 5);
}

#[test]
fn a_bodiless_frame_is_a_call_with_no_arguments() {
    // There is no "read" any more, so an empty frame is not a different
    // operation — it is the same call with `None` for its parameters, and the
    // function decides what that means. This one refuses.
    let registry = adder();
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = serve(&router, "/add", &request_empty("/add"));
    assert_eq!(response.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn a_beve_body_reaches_the_same_function() {
    // The frame header picks the format per request; the function is declared
    // once and reads either. This is the property that made a transcode step
    // unnecessary.
    let registry = adder();
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = serve(
        &router,
        "/add",
        &request_beve("/add", &Operands { a: 20, b: 22 }),
    );
    assert_eq!(response.header.ec, ErrorCode::Ok as u32);
    // Read through `decode_body`, which follows the header: the answer to a
    // BEVE request comes back in BEVE, which the next test is about.
    assert_eq!(response.decode_body::<i64>().expect("body"), 42);
}

#[test]
fn a_response_is_answered_in_the_format_the_request_asked_for() {
    let registry = adder();
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = serve(
        &router,
        "/add",
        &request_beve("/add", &Operands { a: 1, b: 2 }),
    );
    assert_eq!(response.header.body_format, BodyFormat::Beve as u16);
    assert_eq!(response.beve_body::<i64>().expect("beve body"), 3);
}

#[test]
fn a_prefix_routes_only_matching_paths() {
    let registry = adder();
    let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));
    assert!(router.get("/add").is_none());

    let response = serve(
        &router,
        "/api/v1/add",
        &request_json("/api/v1/add", &Operands { a: 4, b: 5 }),
    );
    assert_eq!(response.header.ec, ErrorCode::Ok as u32);
    assert_eq!(response.json_body::<i64>().expect("json body"), 9);
}

#[test]
fn an_escaped_pointer_registers_and_resolves() {
    // `~1` is a literal `/` inside one reference token. Registration stores the
    // canonical key and dispatch has to rebuild the byte-identical one, which
    // is the branch a borrow-the-wire-pointer fast path cannot cover.
    let registry = Arc::new(Registry::new());
    registry
        .register_function("/a~1b/run", |_: Option<RequestBody<'_>>| {
            Ok("slashed".to_string())
        })
        .expect("register escaped-slash function");
    registry
        .register_function("/m~0n", |_: Option<RequestBody<'_>>| Ok("tilded".to_string()))
        .expect("register escaped-tilde function");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    for (path, expected) in [("/a~1b/run", "slashed"), ("/m~0n", "tilded")] {
        let response = serve(&router, path, &request_empty(path));
        assert_eq!(response.header.ec, ErrorCode::Ok as u32, "{path}");
        assert_eq!(response.json_body::<String>().expect("json body"), expected);
    }
}

#[test]
fn an_unknown_body_format_is_invalid_body() {
    let registry = adder();
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let mut request = request_json("/add", &Operands { a: 1, b: 1 });
    request.header.body_format = 7777;

    let response = serve(&router, "/add", &request);
    assert_eq!(response.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn an_invalid_utf8_body_is_invalid_body() {
    let registry = adder();
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let request = Message::builder()
        .id(1)
        .query_str("/add")
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(vec![0xFF, 0xFE])
        .body_format(BodyFormat::Utf8)
        .build();

    let response = serve(&router, "/add", &request);
    assert_eq!(response.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn a_function_error_code_propagates() {
    let registry = Arc::new(Registry::new());
    registry
        .register_function("/boom", |_: Option<RequestBody<'_>>| {
            Err::<(), _>((ErrorCode::ApplicationErrorBase, "denied".into()))
        })
        .expect("register function");
    let router = Router::new().with_registry("", Arc::clone(&registry));

    let response = serve(&router, "/boom", &request_json("/boom", &Operands::default()));
    assert_eq!(response.header.ec, ErrorCode::ApplicationErrorBase as u32);
    assert_eq!(response.error_message_utf8().as_deref(), Some("denied"));
}

#[test]
fn an_unregistered_pointer_is_method_not_found() {
    let registry = adder();
    let router = Router::new().with_registry("", Arc::clone(&registry));

    // The mount claims the prefix, so the miss surfaces from the registry
    // rather than from the router's route table.
    let response = serve(&router, "/absent", &request_empty("/absent"));
    assert_eq!(response.header.ec, ErrorCode::MethodNotFound as u32);
}

#[test]
fn an_invalid_pointer_is_method_not_found() {
    let registry = Registry::new();
    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    let err = registry
        .call_detached("not/a/pointer", None, &mut out)
        .expect_err("invalid pointer should fail");
    assert_eq!(err.code(), ErrorCode::MethodNotFound);
}

#[test]
fn a_prefix_mount_answers_at_its_own_root() {
    // `/api/v1` and `/api/v1/` both name the mount itself, which maps to the
    // empty pointer. Nothing is registered there, so both miss the same way
    // rather than one 404ing at the router and the other at the registry.
    let registry = adder();
    let router = Router::new().with_registry("/api/v1", Arc::clone(&registry));

    for path in ["/api/v1", "/api/v1/"] {
        let response = serve(&router, path, &request_empty(path));
        assert_eq!(
            response.header.ec,
            ErrorCode::MethodNotFound as u32,
            "{path}"
        );
    }
}

#[test]
fn concurrent_calls_against_one_registry_do_not_race() {
    // The registry's own lock is a `RwLock` over the function table, and a call
    // clones the `Arc` out of it rather than holding the guard across the
    // handler. The handler here mutates shared state under its own lock, which
    // is the arrangement that lets an expensive call run without blocking the
    // table.
    let total = Arc::new(AtomicI64::new(0));
    let counted = Arc::clone(&total);
    let registry = Arc::new(Registry::new());
    registry
        .register_function("/bump", move |_: Option<RequestBody<'_>>| {
            Ok(counted.fetch_add(1, Ordering::SeqCst) + 1)
        })
        .expect("register counter");

    const WORKERS: i64 = 8;
    const CALLS: i64 = 200;

    let workers = (0..WORKERS)
        .map(|_| {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                for _ in 0..CALLS {
                    let mut buf = Vec::new();
                    let mut out = ResponseBody::new(&mut buf);
                    registry
                        .call_detached("/bump", None, &mut out)
                        .expect("call should succeed");
                }
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        worker.join().expect("worker should not panic");
    }

    assert_eq!(total.load(Ordering::SeqCst), WORKERS * CALLS);
}
