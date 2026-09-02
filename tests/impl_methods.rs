//! `#[repe::methods]`: methods reflected off an inherent impl block, multi-argument
//! dispatch, `Result` returns, and the `#[repe(typed)]` numeric read path.

#![cfg(not(target_arch = "wasm32"))]

use repe::constants::{BodyFormat, ErrorCode, QueryFormat};
use repe::{Message, Router};

#[derive(Debug)]
struct DeviceError(&'static str);

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "device fault: {}", self.0)
    }
}

#[derive(Default, repe::RepeStruct)]
#[repe(methods)]
struct Device {
    id: String,
    armed: bool,
    #[repe(typed)]
    samples: [u32; 8],
    #[repe(typed)]
    trace: Vec<f64>,
}
structio::object!(Device { id, armed, samples, trace });

#[repe::methods]
impl Device {
    /// Not an endpoint: no receiver to dispatch on.
    fn with_id(id: &str) -> Self {
        Device {
            id: id.to_string(),
            ..Device::default()
        }
    }

    fn greet(&self) -> String {
        format!("device {}", self.id)
    }

    fn scale(&self, factor: f64, offset: f64) -> f64 {
        factor * 2.0 + offset
    }

    fn label(&self, prefix: String, index: u32, suffix: String) -> String {
        format!("{prefix}-{index}-{suffix}")
    }

    fn set_armed(&mut self, armed: bool) {
        self.armed = armed;
    }

    fn arm(&mut self) -> Result<(), DeviceError> {
        if self.id.is_empty() {
            return Err(DeviceError("unidentified"));
        }
        self.armed = true;
        Ok(())
    }

    fn checked_id(&self) -> Result<String, DeviceError> {
        if self.id.is_empty() {
            Err(DeviceError("unidentified"))
        } else {
            Ok(self.id.clone())
        }
    }

    #[repe(rename = "identity")]
    fn id_string(&self) -> String {
        self.id.clone()
    }

    #[repe(skip)]
    #[allow(dead_code)]
    fn internal_checksum(&self) -> u64 {
        self.samples.iter().map(|s| *s as u64).sum()
    }
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
                .build()
}

fn call(router: &Router, path: &str, request: &Message) -> Message {
    router
        .get(path)
        .unwrap_or_else(|| panic!("no handler for {path}"))
        .handle(request)
        .expect("dispatch")
}

fn parse_body(resp: &Message) -> Value {
    serde_json::from_slice(&resp.body).expect("response body should be JSON")
}

fn router_with(device: Device) -> Router {
    Router::new().with_struct("", device).0
}

#[test]
fn methods_come_from_the_impl_block() {
    let router = router_with(Device::with_id("sensor-42"));

    let resp = call(&router, "/greet", &request_empty("/greet"));
    assert_eq!(parse_body(&resp), json!("device sensor-42"));

    // Renamed by attribute; the original name is not an endpoint.
    let resp = call(&router, "/identity", &request_empty("/identity"));
    assert_eq!(parse_body(&resp), json!("sensor-42"));
    let resp = call(&router, "/id_string", &request_empty("/id_string"));
    assert_eq!(resp.header.ec, ErrorCode::MethodNotFound as u32);

    // `#[repe(skip)]` keeps the method off the wire.
    let resp = call(
        &router,
        "/internal_checksum",
        &request_empty("/internal_checksum"),
    );
    assert_eq!(resp.header.ec, ErrorCode::MethodNotFound as u32);

    // An associated function has no instance to dispatch on.
    let resp = call(&router, "/with_id", &request_empty("/with_id"));
    assert_eq!(resp.header.ec, ErrorCode::MethodNotFound as u32);
}

#[test]
fn mutating_methods_take_the_body_as_their_argument() {
    let (router, handle) = Router::new().with_struct("", Device::with_id("sensor-42"));

    let resp = call(
        &router,
        "/set_armed",
        &request_json("/set_armed", &json!(true)),
    );
    assert_eq!(parse_body(&resp), Value::Null);
    assert!(handle.lock().unwrap().armed);
}

#[test]
fn multi_argument_methods_accept_a_positional_array() {
    let router = router_with(Device::with_id("sensor-42"));

    let resp = call(
        &router,
        "/scale",
        &request_json("/scale", &json!([3.0, 1.5])),
    );
    assert_eq!(parse_body(&resp), json!(7.5));

    let resp = call(
        &router,
        "/label",
        &request_json("/label", &json!(["ch", 7, "raw"])),
    );
    assert_eq!(parse_body(&resp), json!("ch-7-raw"));
}

#[test]
fn multi_argument_methods_accept_an_object_keyed_by_parameter_name() {
    let router = router_with(Device::with_id("sensor-42"));

    let resp = call(
        &router,
        "/scale",
        &request_json("/scale", &json!({"offset": 1.5, "factor": 3.0})),
    );
    assert_eq!(parse_body(&resp), json!(7.5));

    let resp = call(
        &router,
        "/label",
        &request_json(
            "/label",
            &json!({"prefix": "ch", "index": 7, "suffix": "raw"}),
        ),
    );
    assert_eq!(parse_body(&resp), json!("ch-7-raw"));
}

#[test]
fn a_multi_argument_body_of_the_wrong_shape_is_an_error() {
    let router = router_with(Device::with_id("sensor-42"));

    let short = call(&router, "/scale", &request_json("/scale", &json!([3.0])));
    assert_eq!(short.header.ec, ErrorCode::InvalidBody as u32);
    assert!(
        String::from_utf8_lossy(&short.body).contains("expected 2 arguments, got 1"),
        "unexpected error: {}",
        String::from_utf8_lossy(&short.body)
    );

    let scalar = call(&router, "/scale", &request_json("/scale", &json!(3.0)));
    assert_eq!(scalar.header.ec, ErrorCode::InvalidBody as u32);
    assert!(
        String::from_utf8_lossy(&scalar.body).contains("factor, offset"),
        "unexpected error: {}",
        String::from_utf8_lossy(&scalar.body)
    );

    let wrong_type = call(
        &router,
        "/scale",
        &request_json("/scale", &json!({"factor": "three", "offset": 1.5})),
    );
    assert_eq!(wrong_type.header.ec, ErrorCode::InvalidBody as u32);
    assert!(
        String::from_utf8_lossy(&wrong_type.body).contains("/scale(factor)"),
        "unexpected error: {}",
        String::from_utf8_lossy(&wrong_type.body)
    );

    let missing = call(&router, "/scale", &request_empty("/scale"));
    assert_eq!(missing.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn a_result_return_maps_ok_to_the_payload_and_err_to_an_error_frame() {
    let (router, handle) = Router::new().with_struct("", Device::with_id("sensor-42"));

    let ok = call(&router, "/arm", &request_empty("/arm"));
    assert_eq!(ok.header.ec, ErrorCode::Ok as u32);
    assert_eq!(parse_body(&ok), Value::Null);
    assert!(handle.lock().unwrap().armed);

    let ok = call(&router, "/checked_id", &request_empty("/checked_id"));
    assert_eq!(ok.header.ec, ErrorCode::Ok as u32);
    assert_eq!(parse_body(&ok), json!("sensor-42"));

    let (router, _handle) = Router::new().with_struct("", Device::default());
    let err = call(&router, "/arm", &request_empty("/arm"));
    assert_eq!(err.header.ec, ErrorCode::ParseError as u32);
    let message = String::from_utf8_lossy(&err.body);
    assert!(message.contains("/arm"), "unexpected error: {message}");
    assert!(
        message.contains("device fault: unidentified"),
        "unexpected error: {message}"
    );
}

#[test]
fn the_whole_struct_listing_publishes_impl_block_signatures() {
    let router = router_with(Device::with_id("sensor-42"));

    let body = parse_body(&call(&router, "", &request_empty("")));
    assert_eq!(body["id"], json!("sensor-42"));
    assert_eq!(body["greet"], json!("fn(&self) -> String"));
    assert_eq!(
        body["scale"],
        json!("fn(&self, factor: f64, offset: f64) -> f64")
    );
    assert_eq!(body["set_armed"], json!("fn(&mut self, armed: bool) -> ()"));
    assert_eq!(
        body["arm"],
        json!("fn(&mut self) -> Result<(), DeviceError>")
    );
    assert_eq!(body["identity"], json!("fn(&self) -> String"));
    assert!(body.get("id_string").is_none());
    assert!(body.get("internal_checksum").is_none());
    assert!(body.get("with_id").is_none());

    // A typed field is a plain JSON array inside the listing: the frame is
    // already JSON, so the BEVE encoding is reachable only per-field.
    assert_eq!(body["samples"], json!([0, 0, 0, 0, 0, 0, 0, 0]));
}

#[test]
fn a_typed_field_reads_as_a_bulk_beve_array() {
    let mut device = Device::with_id("sensor-42");
    device.samples = [1, 2, 3, 4, 5, 6, 7, 8];
    device.trace = vec![0.5, 1.5, 2.5];
    let router = router_with(device);

    let resp = call(&router, "/samples", &request_empty("/samples"));
    assert_eq!(resp.header.body_format, BodyFormat::Beve as u16);
    assert_eq!(
        resp.decode_typed_slice::<u32>().unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    );

    let resp = call(&router, "/trace", &request_empty("/trace"));
    assert_eq!(resp.header.body_format, BodyFormat::Beve as u16);
    assert_eq!(
        resp.decode_typed_slice::<f64>().unwrap(),
        vec![0.5, 1.5, 2.5]
    );

    // The bytes are exactly what the bulk builder emits for the same slice.
    let expected = Message::builder()
        .body_typed_slice(&[0.5f64, 1.5, 2.5])
        .build();
    assert_eq!(resp.body, expected.body);
}

#[test]
fn a_typed_field_still_accepts_a_json_write() {
    let (router, handle) = Router::new().with_struct("", Device::with_id("sensor-42"));

    let resp = call(
        &router,
        "/trace",
        &request_json("/trace", &json!([9.0, 8.0])),
    );
    assert_eq!(parse_body(&resp), Value::Null);
    assert_eq!(handle.lock().unwrap().trace, vec![9.0, 8.0]);
}

#[test]
fn a_nested_struct_reaches_its_own_impl_block_methods() {
    #[derive(Default, repe::RepeStruct)]
    #[repe(methods)]
    struct Channel {
        gain: f64,
    }
    structio::object!(Channel { gain });

    #[repe::methods]
    impl Channel {
        fn describe(&self) -> String {
            format!("gain={}", self.gain)
        }

        fn blend(&self, a: f64, b: f64) -> f64 {
            self.gain * (a + b)
        }
    }

    #[derive(Default, repe::RepeStruct)]
    struct Rack {
        #[repe(nested)]
        channel: Channel,
    }
    structio::object!(Rack { channel });

    let (router, handle) = Router::new().with_struct("", Rack::default());
    handle.lock().unwrap().channel.gain = 2.0;

    let resp = call(
        &router,
        "/channel/describe",
        &request_empty("/channel/describe"),
    );
    assert_eq!(parse_body(&resp), json!("gain=2"));

    let resp = call(
        &router,
        "/channel/blend",
        &request_json("/channel/blend", &json!([1.0, 3.0])),
    );
    assert_eq!(parse_body(&resp), json!(8.0));

    let listing = parse_body(&call(&router, "", &request_empty("")));
    assert_eq!(listing["channel"]["gain"], json!(2.0));
    assert_eq!(listing["channel"]["describe"], json!("fn(&self) -> String"));

    let missing = call(&router, "/channel/nope", &request_empty("/channel/nope"));
    assert_eq!(missing.header.ec, ErrorCode::MethodNotFound as u32);
    assert!(
        String::from_utf8_lossy(&missing.body).contains("/channel/nope"),
        "unexpected error: {}",
        String::from_utf8_lossy(&missing.body)
    );
}

#[test]
fn a_method_path_rejects_trailing_segments() {
    let router = router_with(Device::with_id("sensor-42"));

    let resp = call(&router, "/greet/extra", &request_empty("/greet/extra"));
    assert_eq!(resp.header.ec, ErrorCode::MethodNotFound as u32);
    assert!(
        String::from_utf8_lossy(&resp.body).contains("/greet/extra"),
        "unexpected error: {}",
        String::from_utf8_lossy(&resp.body)
    );
}

#[test]
fn the_whole_struct_listing_is_in_declaration_order() {
    // The encode path emits keys as declared, which is what Glaze's aggregate
    // reflection does and no longer depends on whether anything in the
    // dependency graph turned on `serde_json/preserve_order`.
    let router = router_with(Device::with_id("sensor-42"));
    let resp = call(&router, "", &request_empty(""));
    let body = String::from_utf8(resp.body).unwrap();

    let order: Vec<&str> = [
        "\"id\"",
        "\"armed\"",
        "\"samples\"",
        "\"trace\"",
        "\"greet\"",
        "\"scale\"",
        "\"label\"",
        "\"set_armed\"",
        "\"arm\"",
        "\"checked_id\"",
        "\"identity\"",
    ]
    .into_iter()
    .collect();

    let mut cursor = 0usize;
    for key in order {
        let at = body[cursor..]
            .find(key)
            .unwrap_or_else(|| panic!("{key} not found after position {cursor} in {body}"));
        cursor += at + key.len();
    }
}

/// The struct-level list is the escape hatch for an impl block that cannot be
/// annotated. It carries the same multi-argument support, and the two forms
/// coexist on one struct.
#[derive(Default, repe::RepeStruct)]
#[repe(methods, methods(listed = combine(&self, left: i32, right: i32) -> i32))]
struct MixedSurface {
    base: i32,
}
structio::object!(MixedSurface { base });

impl MixedSurface {
    fn combine(&self, left: i32, right: i32) -> i32 {
        self.base + left + right
    }
}

#[repe::methods]
impl MixedSurface {
    fn via_impl_block(&self, a: i32, b: i32, c: i32) -> i32 {
        self.base + a + b + c
    }
}

#[test]
fn the_struct_level_list_also_takes_several_arguments() {
    let (router, handle) = Router::new().with_struct("", MixedSurface::default());
    handle.lock().unwrap().base = 10;

    let resp = call(&router, "/listed", &request_json("/listed", &json!([1, 2])));
    assert_eq!(parse_body(&resp), json!(13));

    let resp = call(
        &router,
        "/listed",
        &request_json("/listed", &json!({"left": 1, "right": 2})),
    );
    assert_eq!(parse_body(&resp), json!(13));
}

#[test]
fn a_listed_method_and_an_impl_block_coexist() {
    let (router, handle) = Router::new().with_struct("", MixedSurface::default());
    handle.lock().unwrap().base = 10;

    let resp = call(
        &router,
        "/via_impl_block",
        &request_json("/via_impl_block", &json!([1, 2, 3])),
    );
    assert_eq!(parse_body(&resp), json!(16));

    let listing = parse_body(&call(&router, "", &request_empty("")));
    assert_eq!(listing["base"], json!(10));
    assert_eq!(
        listing["listed"],
        json!("fn(&self, left: i32, right: i32) -> i32")
    );
    assert_eq!(
        listing["via_impl_block"],
        json!("fn(&self, a: i32, b: i32, c: i32) -> i32")
    );
}

#[test]
fn a_beve_request_body_reaches_a_multi_argument_method() {
    let router = router_with(Device::with_id("sensor-42"));

    let request = Message::builder()
        .query_str("/scale")
        .query_format(QueryFormat::JsonPointer)
        .body_beve(&(3.0f64, 1.5f64))
                .build();
    let resp = call(&router, "/scale", &request);
    assert_eq!(parse_body(&resp), json!(7.5));
}

// --- The invariant the whole `Sink` factoring exists to hold ------------------

#[derive(Default, repe::RepeStruct)]
#[repe(methods)]
struct Inner {
    scale: f64,
    #[repe(typed)]
    window: [i16; 4],
    #[repe(readonly)]
    serial: String,
}
structio::object!(Inner { scale, window, serial });

#[repe::methods]
impl Inner {
    fn doubled(&self) -> f64 {
        self.scale * 2.0
    }
}

#[derive(Default, repe::RepeStruct)]
struct Outer {
    name: String,
    #[repe(nested)]
    inner: Inner,
    #[repe(skip)]
    #[allow(dead_code)]
    hidden: u8,
}
structio::object!(Outer { name, inner, hidden });

fn outer() -> Outer {
    Outer {
        name: "rack".into(),
        inner: Inner {
            scale: 1.5,
            window: [1, -2, 3, -4],
            serial: "SN-1".into(),
        },
        hidden: 9,
    }
}

/// `repe_handle` and `repe_handle_into` are generated from one set of specs and
/// must produce the same answer for every arm shape. `#[repe(typed)]` is the one
/// sanctioned exception, pinned separately below.
#[test]
fn the_value_and_encode_paths_agree() {
    use repe::structs::{RepeStruct, ResponseBody};

    let cases: [(&[&str], Option<Value>); 12] = [
        (&[], None),                         // whole object
        (&["name"], None),                   // leaf read
        (&["name"], Some(json!("renamed"))), // leaf write
        (&["inner"], None),                  // nested read
        (
            &["inner"],
            Some(json!({"scale": 4.0, "window": [0, 0, 0, 0], "serial": "SN-2"})),
        ),
        (&["inner", "scale"], None),                 // nested leaf read
        (&["inner", "scale"], Some(json!(9.5))),     // nested leaf write
        (&["inner", "doubled"], None),               // nested impl-block method
        (&["inner", "serial"], Some(json!("nope"))), // readonly rejection
        (&["hidden"], None),                         // skipped field
        (&["name", "extra"], None),                  // InvalidSubpath
        (&["nope"], None),                           // InvalidPath
    ];

    for (segments, body) in cases {
        let mut via_value = outer();
        let mut via_encode = outer();

        let value_result = via_value.repe_handle(segments, body.clone());

        let mut buf = Vec::new();
        let mut out = ResponseBody::new(&mut buf);
        let encode_result = via_encode.repe_handle_into(segments, body.clone(), &mut out);

        match (&value_result, &encode_result) {
            (Ok(value), Ok(())) => {
                let expected = value.clone().unwrap_or(Value::Null);
                let encoded: Value = serde_json::from_slice(&buf)
                    .unwrap_or_else(|e| panic!("{segments:?} produced invalid JSON: {e}"));
                assert_eq!(encoded, expected, "payload diverged at {segments:?}");
            }
            (Err(value_err), Err(encode_err)) => assert_eq!(
                value_err.to_string(),
                encode_err.to_string(),
                "error text diverged at {segments:?}"
            ),
            _ => panic!("outcome diverged at {segments:?}: {value_result:?} vs {encode_result:?}"),
        }

        // The write arms must leave the struct in the same state, too.
        assert_eq!(
            serde_json::to_value(&via_value).unwrap(),
            serde_json::to_value(&via_encode).unwrap(),
            "struct state diverged at {segments:?}"
        );
    }
}

/// The one divergence, asserted rather than assumed: a `Value` has no
/// representation for a BEVE typed body, so `repe_handle` yields the JSON array.
#[test]
fn a_typed_field_is_the_one_sanctioned_divergence() {
    use repe::structs::{RepeStruct, ResponseBody};

    let mut device = outer();
    assert_eq!(
        device.repe_handle(&["inner", "window"], None).unwrap(),
        Some(json!([1, -2, 3, -4])),
        "the Value path has nowhere to put a typed body"
    );

    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    device
        .repe_handle_into(&["inner", "window"], None, &mut out)
        .unwrap();
    assert_eq!(out.format(), BodyFormat::Beve);
    assert_eq!(
        buf,
        Message::builder()
            .body_typed_slice(&[1i16, -2, 3, -4])
            .build()
            .body
    );

    // Inside the listing the frame is already JSON, at any depth.
    let listing = parse_body(&call(
        &Router::new().with_struct("", outer()).0,
        "",
        &request_empty(""),
    ));
    assert_eq!(listing["inner"]["window"], json!([1, -2, 3, -4]));
}

/// A `Result` under an alias that is not *named* `Result` cannot be recognized
/// by a macro, so `Err` ships as data. Pinned so the boundary is a decision on
/// record rather than a surprise; see `docs/server.md`.
#[test]
fn a_result_alias_not_named_result_is_serialized_as_data() {
    type DeviceResult<T> = std::result::Result<T, String>;

    #[derive(Default, repe::RepeStruct)]
    #[repe(methods)]
    struct Aliased {
        v: i32,
    }
    structio::object!(Aliased { v });

    #[repe::methods]
    impl Aliased {
        fn spelled(&self) -> Result<i32, String> {
            Err("spelled".into())
        }
        fn aliased(&self) -> DeviceResult<i32> {
            Err("aliased".into())
        }
    }

    let router = Router::new().with_struct("", Aliased::default()).0;

    let spelled = call(&router, "/spelled", &request_empty("/spelled"));
    assert_eq!(spelled.header.ec, ErrorCode::ParseError as u32);

    let aliased = call(&router, "/aliased", &request_empty("/aliased"));
    assert_eq!(aliased.header.ec, ErrorCode::Ok as u32);
    assert_eq!(parse_body(&aliased), json!({"Err": "aliased"}));
}
