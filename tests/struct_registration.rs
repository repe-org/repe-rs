#![allow(unreachable_code)]

use repe::constants::{ErrorCode, QueryFormat};
use repe::{Message, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
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

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods(
    hello(&self) -> String,
    world(&self) -> String,
    get_number(&self) -> i32
))]
struct MetaFunctions {}

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

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
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

impl MyNestedFunctions {
    fn append_awesome(&self, input: String) -> String {
        format!("{input} awesome!")
    }
}

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods(
    get_name(&self) -> String,
    set_name(&mut self, new_name: String) -> (),
    custom_name = set_name(&mut self, new_name: String) -> ()
))]
struct ExampleFunctions {
    name: String,
}

impl ExampleFunctions {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&mut self, new_name: String) {
        self.name = new_name;
    }
}

#[allow(unreachable_code)]
#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
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

impl AttributeStruct {
    fn describe(&self) -> String {
        format!("{}:{}", self.name, self.value)
    }
}

#[derive(Default, Serialize, Deserialize, repe::RepeStruct)]
struct RootStruct {
    foo: i32,
    bar: String,
}

fn request_empty(path: &str) -> Message {
    Message::builder()
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .build()
}

fn request_json(path: &str, body: &Value) -> Message {
    Message::builder()
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(body)
        .unwrap()
        .build()
}

fn parse_body(resp: &Message) -> Value {
    serde_json::from_slice(&resp.body).expect("response body should be JSON")
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
    assert_eq!(parse_body(&resp), Value::from(55));

    // writing integer resets to requested value
    let handler = router.get("/i").unwrap();
    let resp = handler
        .handle(&request_json("/i", &json!(42)))
        .expect("write integer");
    assert_eq!(parse_body(&resp), Value::Null);
    assert_eq!(shared.lock().unwrap().i, 42);

    // zero-argument functions
    let hello = router
        .get("/hello")
        .unwrap()
        .handle(&request_empty("/hello"))
        .unwrap();
    assert_eq!(parse_body(&hello), Value::String("Hello".into()));
    let world = router
        .get("/world")
        .unwrap()
        .handle(&request_empty("/world"))
        .unwrap();
    assert_eq!(parse_body(&world), Value::String("World".into()));

    // zero-arg with body should still work
    let get_number = router
        .get("/get_number")
        .unwrap()
        .handle(&request_json("/get_number", &json!("ignored")))
        .unwrap();
    assert_eq!(parse_body(&get_number), Value::from(42));

    // void function returns null
    let void_resp = router
        .get("/void_func")
        .unwrap()
        .handle(&request_empty("/void_func"))
        .unwrap();
    assert_eq!(parse_body(&void_resp), Value::Null);

    // max with parameters
    let max_resp = router
        .get("/max")
        .unwrap()
        .handle(&request_json("/max", &json!([1.1, 3.3, 2.25])))
        .unwrap();
    assert_eq!(parse_body(&max_resp), Value::from(3.3));

    // root snapshot
    let snapshot = router.get("").unwrap().handle(&request_empty("")).unwrap();
    let body = parse_body(&snapshot);
    assert_eq!(body["i"], Value::from(42));
    assert_eq!(body["hello"], Value::String("fn(&self) -> String".into()));
    assert_eq!(
        body["max"],
        Value::String("fn(&self, vec: Vec<f64>) -> f64".into())
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
    assert_eq!(parse_body(&resp), Value::Null);

    let hello = router
        .get("/my_functions/hello")
        .unwrap()
        .handle(&request_empty("/my_functions/hello"))
        .unwrap();
    assert_eq!(parse_body(&hello), Value::String("Hello".into()));

    let meta = router
        .get("/meta_functions/hello")
        .unwrap()
        .handle(&request_empty("/meta_functions/hello"))
        .unwrap();
    assert_eq!(parse_body(&meta), Value::String("Hello".into()));

    let append = router
        .get("/append_awesome")
        .unwrap()
        .handle(&request_json("/append_awesome", &json!("you are")))
        .unwrap();
    assert_eq!(
        parse_body(&append),
        Value::String("you are awesome!".into())
    );

    let write_string = router
        .get("/my_string")
        .unwrap()
        .handle(&request_json("/my_string", &json!("Howdy!")))
        .unwrap();
    assert_eq!(parse_body(&write_string), Value::Null);

    let read_string = router
        .get("/my_string")
        .unwrap()
        .handle(&request_empty("/my_string"))
        .unwrap();
    assert_eq!(parse_body(&read_string), Value::String("Howdy!".into()));

    shared.lock().unwrap().my_string.clear();
    let empty_read = router
        .get("/my_string")
        .unwrap()
        .handle(&request_empty("/my_string"))
        .unwrap();
    assert_eq!(parse_body(&empty_read), Value::String("".into()));

    let max_resp = router
        .get("/my_functions/max")
        .unwrap()
        .handle(&request_json("/my_functions/max", &json!([1.1, 3.3, 2.25])))
        .unwrap();
    assert_eq!(parse_body(&max_resp), Value::from(3.3));

    let my_functions_snapshot = router
        .get("/my_functions")
        .unwrap()
        .handle(&request_empty("/my_functions"))
        .unwrap();
    let snapshot_body = parse_body(&my_functions_snapshot);
    assert_eq!(snapshot_body["i"], Value::from(0));
    assert_eq!(
        snapshot_body["hello"],
        Value::String("fn(&self) -> String".into())
    );

    let full_snapshot = router.get("").unwrap().handle(&request_empty("")).unwrap();
    let body = parse_body(&full_snapshot);
    assert_eq!(body["my_string"], Value::String("".into()));
    assert!(body["my_functions"].is_object());
    assert!(body["meta_functions"].is_object());
}

#[test]
fn example_functions() {
    let shared = Arc::new(Mutex::new(ExampleFunctions::default()));
    let router = Router::new().with_struct_shared("", shared.clone());

    let write_name = router
        .get("/name")
        .unwrap()
        .handle(&request_json("/name", &json!("Susan")))
        .unwrap();
    assert_eq!(parse_body(&write_name), Value::Null);

    let read_name = router
        .get("/get_name")
        .unwrap()
        .handle(&request_empty("/get_name"))
        .unwrap();
    assert_eq!(parse_body(&read_name), Value::String("Susan".into()));

    let read_with_body = router
        .get("/get_name")
        .unwrap()
        .handle(&request_json("/get_name", &json!("Bob")))
        .unwrap();
    assert_eq!(parse_body(&read_with_body), Value::String("Susan".into()));

    assert_eq!(shared.lock().unwrap().name, "Susan");

    let set_name = router
        .get("/set_name")
        .unwrap()
        .handle(&request_json("/set_name", &json!("Bob")))
        .unwrap();
    assert_eq!(parse_body(&set_name), Value::Null);
    assert_eq!(shared.lock().unwrap().name, "Bob");

    let custom_name = router
        .get("/custom_name")
        .unwrap()
        .handle(&request_json("/custom_name", &json!("Alice")))
        .unwrap();
    assert_eq!(parse_body(&custom_name), Value::Null);
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
    assert_eq!(parse_body(&get_name), Value::String("Initial".into()));

    let set_name = router
        .get("/set_name")
        .unwrap()
        .handle(&request_json("/set_name", &json!("Updated")))
        .unwrap();
    assert_eq!(parse_body(&set_name), Value::Null);

    assert_eq!(shared.read().unwrap().name, "Updated");
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
    assert_eq!(parse_body(&read_value), Value::from(7));

    let write_value = router
        .get("/renamed_value")
        .unwrap()
        .handle(&request_json("/renamed_value", &json!(11)))
        .unwrap();
    assert_eq!(parse_body(&write_value), Value::Null);
    assert_eq!(shared.lock().unwrap().value, 11);

    // readonly field rejects writes but stays readable
    let readonly_err = router
        .get("/name")
        .unwrap()
        .handle(&request_json("/name", &json!("beta")))
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
    assert_eq!(parse_body(&readonly_value), Value::String("alpha".into()));

    // root snapshot omits skipped field and reflects methods
    let snapshot = router.get("").unwrap().handle(&request_empty("")).unwrap();
    let body = parse_body(&snapshot);
    assert_eq!(body["renamed_value"], Value::from(11));
    assert_eq!(body["name"], Value::String("alpha".into()));
    assert!(
        body.get("hidden").is_none(),
        "skip attribute should hide field"
    );
    assert_eq!(body["alias"], Value::String("fn(&self) -> String".into()));

    // calling alias method reflects updated state
    let alias_value = router
        .get("/alias")
        .unwrap()
        .handle(&request_empty("/alias"))
        .unwrap();
    assert_eq!(parse_body(&alias_value), Value::String("alpha:11".into()));

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
        .body_json(&json!({"foo": 5, "bar": "five"}))
        .unwrap()
        .build();
    let resp = router.get("").unwrap().handle(&replace).unwrap();
    assert_eq!(parse_body(&resp), Value::Null);

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
    assert_eq!(parse_body(&number), Value::from(42));

    let write = router
        .get("/sub/i")
        .unwrap()
        .handle(&request_json("/sub/i", &json!(9)))
        .unwrap();
    assert_eq!(parse_body(&write), Value::Null);
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
