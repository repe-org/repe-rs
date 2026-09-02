#![cfg(not(target_arch = "wasm32"))]
#![allow(unreachable_code)]

use repe::constants::{BodyFormat, ErrorCode, QueryFormat};
use repe::{Message, Router};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock};

#[cfg(feature = "parking-lot")]
use parking_lot::RwLock as ParkingRwLock;

#[derive(Default, repe::RepeStruct)]
#[repe(methods(
    hello(&self) -> String,
    world(&self) -> String,
    get_number(&self) -> i32,
    void_func(&mut self) -> (),
    max(&self, vec: Vec<f64>) -> f64
))]
struct MyFunctions {
    i: i32,
}
structio::object!(MyFunctions { i });

impl MyFunctions {
    fn hello(&self) -> String {
        "Hello".into()
    }

    fn world(&self) -> String {
        "World".into()
    }

    fn get_number(&self) -> i32 {
        42
    }

    fn void_func(&mut self) {}

    fn max(&self, vec: Vec<f64>) -> f64 {
        vec.into_iter().fold(f64::NEG_INFINITY, f64::max)
    }
}

#[derive(Default, repe::RepeStruct)]
#[repe(methods(
    hello(&self) -> String,
    world(&self) -> String,
    get_number(&self) -> i32
))]
struct MetaFunctions {}
structio::object!(MetaFunctions {});

impl MetaFunctions {
    fn hello(&self) -> String {
        "Hello".into()
    }

    fn world(&self) -> String {
        "World".into()
    }

    fn get_number(&self) -> i32 {
        42
    }
}

#[derive(Default, repe::RepeStruct)]
#[repe(methods(
    append_awesome(&self, input: String) -> String
))]
struct MyNestedFunctions {
    #[repe(nested)]
    my_functions: MyFunctions,
    #[repe(nested)]
    meta_functions: MetaFunctions,
    my_string: String,
}
structio::object!(MyNestedFunctions {
    my_functions,
    meta_functions,
    my_string
});

impl MyNestedFunctions {
    fn append_awesome(&self, input: String) -> String {
        format!("{input} awesome!")
    }
}

#[derive(Default, repe::RepeStruct)]
#[repe(methods(
    get_name(&self) -> String,
    set_name(&mut self, new_name: String) -> (),
    custom_name = set_name(&mut self, new_name: String) -> ()
))]
struct ExampleFunctions {
    name: String,
}
structio::object!(ExampleFunctions { name });

impl ExampleFunctions {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&mut self, new_name: String) {
        self.name = new_name;
    }
}

#[allow(unreachable_code)]
#[derive(Default, repe::RepeStruct)]
#[repe(methods(
    alias = describe(&self) -> String
))]
struct AttributeStruct {
    #[repe(rename = "renamed_value")]
    value: i32,
    #[repe(skip)]
    hidden: bool,
    #[repe(readonly)]
    name: String,
}
structio::object!(AttributeStruct {
    value,
    hidden,
    name
});

impl AttributeStruct {
    fn describe(&self) -> String {
        format!("{}:{}", self.name, self.value)
    }
}

#[derive(Default, repe::RepeStruct)]
struct RootStruct {
    foo: i32,
    bar: String,
}
structio::object!(RootStruct { foo, bar });

fn request_empty(path: &str) -> Message {
    Message::builder()
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .build()
}

/// A request whose body is the given JSON text, sent verbatim.
///
/// These tests are about the wire contract, so the body is written as the text
/// a client would send rather than built from a declared type. A declared type
/// would put a shape between the test and what it is testing, and the malformed
/// cases could not be expressed at all.
fn request_json(path: &str, body: &str) -> Message {
    Message::builder()
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(body.as_bytes().to_vec())
        .body_format(BodyFormat::Json)
        .build()
}

/// The response body as JSON text.
///
/// Compared as text throughout, which is what a response *is*: with no document
/// model there is nothing between the bytes and the assertion, and the key
/// order a listing emits becomes assertable rather than normalized away.
fn body_text(resp: &Message) -> &str {
    std::str::from_utf8(&resp.body).expect("response body should be UTF-8 JSON")
}

fn request_raw(path: &str, bytes: &[u8]) -> Message {
    Message::builder()
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(bytes.to_vec())
        .build()
}

#[test]
fn structs_of_functions() {
    let shared = Arc::new(Mutex::new(MyFunctions::default()));
    {
        let mut guard = shared.lock().unwrap();
        guard.i = 55;
    }

    let router = Router::new().with_struct_shared("", shared.clone());

    // read integer field
    let handler = router.get("/i").expect("handler for /i");
    let resp = handler.handle(&request_empty("/i")).unwrap();
    assert_eq!(body_text(&resp), "55");

    // writing integer resets to requested value
    let handler = router.get("/i").unwrap();
    let resp = handler
        .handle(&request_json("/i", r##"42"##))
        .expect("write integer");
    assert_eq!(body_text(&resp), "null");
    assert_eq!(shared.lock().unwrap().i, 42);

    // zero-argument functions
    let hello = router
        .get("/hello")
        .unwrap()
        .handle(&request_empty("/hello"))
        .unwrap();
    assert_eq!(body_text(&hello), r#""Hello""#);
    let world = router
        .get("/world")
        .unwrap()
        .handle(&request_empty("/world"))
        .unwrap();
    assert_eq!(body_text(&world), r#""World""#);

    // zero-arg with body should still work
    let get_number = router
        .get("/get_number")
        .unwrap()
        .handle(&request_json("/get_number", r##""ignored""##))
        .unwrap();
    assert_eq!(body_text(&get_number), "42");

    // void function returns null
    let void_resp = router
        .get("/void_func")
        .unwrap()
        .handle(&request_empty("/void_func"))
        .unwrap();
    assert_eq!(body_text(&void_resp), "null");

    // max with parameters
    let max_resp = router
        .get("/max")
        .unwrap()
        .handle(&request_json("/max", r##"[1.1, 3.3, 2.25]"##))
        .unwrap();
    assert_eq!(body_text(&max_resp), "3.3");

    // root snapshot
    let snapshot = router.get("").unwrap().handle(&request_empty("")).unwrap();
    let body = body_text(&snapshot);
    assert!(body.contains(r#""i":42"#), "{body}");
    assert!(body.contains(r#""hello":"fn(&self) -> String""#), "{body}");
    assert!(
        body.contains(r#""max":"fn(&self, vec: Vec<f64>) -> f64""#),
        "{body}"
    );
}

#[test]
fn nested_structs_of_functions() {
    let shared = Arc::new(Mutex::new(MyNestedFunctions::default()));

    let router = Router::new().with_struct_shared("", shared.clone());

    // void function reflected in nested struct
    let resp = router
        .get("/my_functions/void_func")
        .unwrap()
        .handle(&request_empty("/my_functions/void_func"))
        .unwrap();
    assert_eq!(body_text(&resp), "null");

    let hello = router
        .get("/my_functions/hello")
        .unwrap()
        .handle(&request_empty("/my_functions/hello"))
        .unwrap();
    assert_eq!(body_text(&hello), r#""Hello""#);

    let meta = router
        .get("/meta_functions/hello")
        .unwrap()
        .handle(&request_empty("/meta_functions/hello"))
        .unwrap();
    assert_eq!(body_text(&meta), r#""Hello""#);

    let append = router
        .get("/append_awesome")
        .unwrap()
        .handle(&request_json("/append_awesome", r##""you are""##))
        .unwrap();
    assert_eq!(body_text(&append), r#""you are awesome!""#);

    let write_string = router
        .get("/my_string")
        .unwrap()
        .handle(&request_json("/my_string", r##""Howdy!""##))
        .unwrap();
    assert_eq!(body_text(&write_string), "null");

    let read_string = router
        .get("/my_string")
        .unwrap()
        .handle(&request_empty("/my_string"))
        .unwrap();
    assert_eq!(body_text(&read_string), r#""Howdy!""#);

    shared.lock().unwrap().my_string.clear();
    let empty_read = router
        .get("/my_string")
        .unwrap()
        .handle(&request_empty("/my_string"))
        .unwrap();
    assert_eq!(body_text(&empty_read), r#""""#);

    let max_resp = router
        .get("/my_functions/max")
        .unwrap()
        .handle(&request_json("/my_functions/max", r##"[1.1, 3.3, 2.25]"##))
        .unwrap();
    assert_eq!(body_text(&max_resp), "3.3");

    let my_functions_snapshot = router
        .get("/my_functions")
        .unwrap()
        .handle(&request_empty("/my_functions"))
        .unwrap();
    let snapshot_body = body_text(&my_functions_snapshot);
    assert!(snapshot_body.contains(r#""i":0"#), "{snapshot_body}");
    assert!(
        snapshot_body.contains(r#""hello":"fn(&self) -> String""#),
        "{snapshot_body}"
    );

    let full_snapshot = router.get("").unwrap().handle(&request_empty("")).unwrap();
    let body = body_text(&full_snapshot);
    assert!(body.contains(r#""my_string":"""#), "{body}");
    assert!(body.contains(r#""my_functions":{"#), "{body}");
    assert!(body.contains(r#""meta_functions":{"#), "{body}");
}

#[test]
fn example_functions() {
    let shared = Arc::new(Mutex::new(ExampleFunctions::default()));
    let router = Router::new().with_struct_shared("", shared.clone());

    let write_name = router
        .get("/name")
        .unwrap()
        .handle(&request_json("/name", r##""Susan""##))
        .unwrap();
    assert_eq!(body_text(&write_name), "null");

    let read_name = router
        .get("/get_name")
        .unwrap()
        .handle(&request_empty("/get_name"))
        .unwrap();
    assert_eq!(body_text(&read_name), r#""Susan""#);

    let read_with_body = router
        .get("/get_name")
        .unwrap()
        .handle(&request_json("/get_name", r##""Bob""##))
        .unwrap();
    assert_eq!(body_text(&read_with_body), r#""Susan""#);

    assert_eq!(shared.lock().unwrap().name, "Susan");

    let set_name = router
        .get("/set_name")
        .unwrap()
        .handle(&request_json("/set_name", r##""Bob""##))
        .unwrap();
    assert_eq!(body_text(&set_name), "null");
    assert_eq!(shared.lock().unwrap().name, "Bob");

    let custom_name = router
        .get("/custom_name")
        .unwrap()
        .handle(&request_json("/custom_name", r##""Alice""##))
        .unwrap();
    assert_eq!(body_text(&custom_name), "null");
    assert_eq!(shared.lock().unwrap().name, "Alice");
}

#[test]
fn struct_shared_accepts_rwlock() {
    let shared = Arc::new(RwLock::new(ExampleFunctions::default()));
    {
        let mut guard = shared.write().unwrap();
        guard.set_name("Initial".into());
    }

    let router = Router::new().with_struct_shared("", shared.clone());

    let get_name = router
        .get("/get_name")
        .unwrap()
        .handle(&request_empty("/get_name"))
        .unwrap();
    assert_eq!(body_text(&get_name), r#""Initial""#);

    let set_name = router
        .get("/set_name")
        .unwrap()
        .handle(&request_json("/set_name", r##""Updated""##))
        .unwrap();
    assert_eq!(body_text(&set_name), "null");

    assert_eq!(shared.read().unwrap().name.as_str(), "Updated");
}

#[test]
fn struct_shared_accepts_tokio_mutex() {
    let shared = Arc::new(TokioMutex::new(ExampleFunctions::default()));
    {
        let mut guard = shared.blocking_lock();
        guard.set_name("Initial".into());
    }

    let router = Router::new().with_struct_shared("", shared.clone());

    let get_name = router
        .get("/get_name")
        .unwrap()
        .handle(&request_empty("/get_name"))
        .unwrap();
    assert_eq!(body_text(&get_name), r#""Initial""#);

    let set_name = router
        .get("/set_name")
        .unwrap()
        .handle(&request_json("/set_name", r##""Updated""##))
        .unwrap();
    assert_eq!(body_text(&set_name), "null");

    assert_eq!(shared.blocking_lock().name.as_str(), "Updated");
}

#[test]
fn struct_shared_accepts_tokio_rwlock() {
    let shared = Arc::new(TokioRwLock::new(ExampleFunctions::default()));
    {
        let mut guard = shared.blocking_write();
        guard.set_name("Initial".into());
    }

    let router = Router::new().with_struct_shared("", shared.clone());

    let get_name = router
        .get("/get_name")
        .unwrap()
        .handle(&request_empty("/get_name"))
        .unwrap();
    assert_eq!(body_text(&get_name), r#""Initial""#);

    let set_name = router
        .get("/set_name")
        .unwrap()
        .handle(&request_json("/set_name", r##""Updated""##))
        .unwrap();
    assert_eq!(body_text(&set_name), "null");

    let guard = shared.blocking_read();
    assert_eq!(guard.name.as_str(), "Updated");
}

#[cfg(feature = "parking-lot")]
#[test]
fn struct_shared_accepts_parking_lot_rwlock() {
    let shared = Arc::new(ParkingRwLock::new(ExampleFunctions::default()));
    {
        let mut guard = shared.write();
        guard.set_name("Initial".into());
    }

    let router = Router::new().with_struct_shared("", shared.clone());

    let get_name = router
        .get("/get_name")
        .unwrap()
        .handle(&request_empty("/get_name"))
        .unwrap();
    assert_eq!(body_text(&get_name), r#""Initial""#);

    let set_name = router
        .get("/set_name")
        .unwrap()
        .handle(&request_json("/set_name", r##""Updated""##))
        .unwrap();
    assert_eq!(body_text(&set_name), "null");

    let guard = shared.read();
    assert_eq!(guard.name.as_str(), "Updated");
}

#[test]
fn struct_attributes_control_behavior() {
    let mut router = Router::new();
    let shared = router.register_struct(
        "",
        AttributeStruct {
            value: 7,
            hidden: true,
            name: "alpha".into(),
        },
    );

    // renamed field is accessible and writable
    let read_value = router
        .get("/renamed_value")
        .unwrap()
        .handle(&request_empty("/renamed_value"))
        .unwrap();
    assert_eq!(body_text(&read_value), "7");

    let write_value = router
        .get("/renamed_value")
        .unwrap()
        .handle(&request_json("/renamed_value", r##"11"##))
        .unwrap();
    assert_eq!(body_text(&write_value), "null");
    assert_eq!(shared.lock().unwrap().value, 11);

    // readonly field rejects writes but stays readable
    let readonly_err = router
        .get("/name")
        .unwrap()
        .handle(&request_json("/name", r##""beta""##))
        .unwrap();
    assert!(readonly_err.is_error());
    assert_eq!(readonly_err.header.ec, ErrorCode::InvalidBody as u32);
    assert!(readonly_err.body_utf8().contains("body not allowed"));
    assert_eq!(shared.lock().unwrap().name, "alpha");

    let readonly_value = router
        .get("/name")
        .unwrap()
        .handle(&request_empty("/name"))
        .unwrap();
    assert_eq!(body_text(&readonly_value), r#""alpha""#);

    // root snapshot omits skipped field and reflects methods
    let snapshot = router.get("").unwrap().handle(&request_empty("")).unwrap();
    let body = body_text(&snapshot);
    assert!(body.contains(r#""renamed_value":11"#), "{body}");
    assert!(body.contains(r#""name":"alpha""#), "{body}");
    assert!(!body.contains("hidden"), "skip attribute should hide field");
    assert!(body.contains(r#""alias":"fn(&self) -> String""#), "{body}");

    // calling alias method reflects updated state
    let alias_value = router
        .get("/alias")
        .unwrap()
        .handle(&request_empty("/alias"))
        .unwrap();
    assert_eq!(body_text(&alias_value), r#""alpha:11""#);

    // raw binary payloads are rejected
    let raw_error = router
        .get("/renamed_value")
        .unwrap()
        .handle(&request_raw("/renamed_value", b"\x01\x02"))
        .unwrap();
    assert!(raw_error.is_error());
    assert_eq!(raw_error.header.ec, ErrorCode::InvalidBody as u32);
    assert_eq!(shared.lock().unwrap().value, 11);
}

#[test]
fn root_write_replaces_struct() {
    let mut router = Router::new();
    let shared = router.register_struct(
        "",
        RootStruct {
            foo: 1,
            bar: "one".into(),
        },
    );

    let replace = Message::builder()
        .query_str("")
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(r##"{"foo": 5, "bar": "five"}"##.as_bytes().to_vec())
        .body_format(BodyFormat::Json)
        .build();
    let resp = router.get("").unwrap().handle(&replace).unwrap();
    assert_eq!(body_text(&resp), "null");

    let data = shared.lock().unwrap();
    assert_eq!(data.foo, 5);
    assert_eq!(data.bar, "five");
}

#[test]
fn struct_with_root_prefix_routes() {
    let shared = Arc::new(Mutex::new(MyFunctions::default()));
    let router = Router::new().with_struct_shared("sub", shared.clone());

    let number = router
        .get("/sub/get_number")
        .unwrap()
        .handle(&request_empty("/sub/get_number"))
        .unwrap();
    assert_eq!(body_text(&number), "42");

    let write = router
        .get("/sub/i")
        .unwrap()
        .handle(&request_json("/sub/i", r##"9"##))
        .unwrap();
    assert_eq!(body_text(&write), "null");
    assert_eq!(shared.lock().unwrap().i, 9);

    let invalid = router
        .get("/sub/unknown")
        .unwrap()
        .handle(&request_empty("/sub/unknown"))
        .unwrap();
    assert!(invalid.is_error());
    assert_eq!(invalid.header.ec, ErrorCode::MethodNotFound as u32);
    assert!(invalid.body_utf8().contains("invalid path"));

    assert!(router.get("/i").is_none(), "prefix should scope visibility");
}

/// Pins down the RFC 6901 split between `""` and `"/"` at the dispatch
/// boundary so the routing fast path does not silently collapse them.
///
/// * `""` → empty pointer → no reference tokens → serialize the whole struct.
/// * `"/"` → pointer with a single empty reference token → field named `""`,
///   which the derive-macro emits as `InvalidPath` because no such field
///   exists.
///
/// Holds for both a root-mounted struct (`""`) and a prefix-mounted struct
/// (`"/svc"`), where the trailing-slash form is `"/svc/"`.
#[test]
fn struct_dispatch_distinguishes_root_from_trailing_slash() {
    // Root-mounted struct: "" → whole struct, "/" → InvalidPath.
    let root_shared = Arc::new(Mutex::new(RootStruct {
        foo: 7,
        bar: "ok".into(),
    }));
    let root_router = Router::new().with_struct_shared("", root_shared);

    let whole = root_router
        .get("")
        .unwrap()
        .handle(&request_empty(""))
        .unwrap();
    assert!(!whole.is_error(), "empty query should return the struct");
    assert_eq!(body_text(&whole), r##"{"foo":7,"bar":"ok"}"##);

    let trailing_root = root_router
        .get("/")
        .unwrap()
        .handle(&request_empty("/"))
        .unwrap();
    assert!(
        trailing_root.is_error(),
        "lone `/` should resolve to the empty-named field and miss"
    );
    assert_eq!(trailing_root.header.ec, ErrorCode::MethodNotFound as u32);

    // Prefix-mounted struct: "/svc" → whole struct, "/svc/" → InvalidPath.
    let svc_shared = Arc::new(Mutex::new(RootStruct {
        foo: 1,
        bar: "x".into(),
    }));
    let svc_router = Router::new().with_struct_shared("/svc", svc_shared);

    let svc_whole = svc_router
        .get("/svc")
        .unwrap()
        .handle(&request_empty("/svc"))
        .unwrap();
    assert!(!svc_whole.is_error(), "/svc should return the struct");
    assert_eq!(body_text(&svc_whole), r##"{"foo":1,"bar":"x"}"##);

    let svc_trailing = svc_router
        .get("/svc/")
        .unwrap()
        .handle(&request_empty("/svc/"))
        .unwrap();
    assert!(
        svc_trailing.is_error(),
        "/svc/ should resolve to the empty-named field and miss"
    );
    assert_eq!(svc_trailing.header.ec, ErrorCode::MethodNotFound as u32);
}
