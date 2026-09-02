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
structio::object!(Device {
    id,
    armed,
    samples,
    trace
});

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

/// A request whose body is the given JSON text, sent verbatim.
///
/// These tests are about the *wire* contract — which argument forms dispatch
/// accepts, and what a wrong one does — so the body is written as the text a
/// client would send rather than built from a declared type. Declaring one
/// would put a shape between the test and the thing it is testing, and the
/// malformed cases could not be expressed at all.
fn request_json(path: &str, body: &str) -> Message {
    Message::builder()
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(body.as_bytes().to_vec())
        .body_format(BodyFormat::Json)
        .build()
}

fn call(router: &Router, path: &str, request: &Message) -> Message {
    router
        .get(path)
        .unwrap_or_else(|| panic!("no handler for {path}"))
        .handle(request)
        .expect("dispatch")
}

/// The response body as JSON text.
///
/// Compared as text throughout, which is what a response *is*: with no document
/// model there is nothing between the bytes and the assertion, and the key
/// order a listing emits becomes assertable rather than being normalized away.
fn body_text(resp: &Message) -> &str {
    std::str::from_utf8(&resp.body).expect("response body should be UTF-8 JSON")
}

fn router_with(device: Device) -> Router {
    Router::new().with_struct("", device).0
}

#[test]
fn methods_come_from_the_impl_block() {
    let router = router_with(Device::with_id("sensor-42"));

    let resp = call(&router, "/greet", &request_empty("/greet"));
    assert_eq!(body_text(&resp), r#""device sensor-42""#);

    // Renamed by attribute; the original name is not an endpoint.
    let resp = call(&router, "/identity", &request_empty("/identity"));
    assert_eq!(body_text(&resp), r#""sensor-42""#);
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
        &request_json("/set_armed", r##"true"##),
    );
    assert_eq!(body_text(&resp), "null");
    assert!(handle.lock().unwrap().armed);
}

#[test]
fn multi_argument_methods_accept_a_positional_array() {
    let router = router_with(Device::with_id("sensor-42"));

    let resp = call(
        &router,
        "/scale",
        &request_json("/scale", r##"[3.0, 1.5]"##),
    );
    assert_eq!(body_text(&resp), "7.5");

    let resp = call(
        &router,
        "/label",
        &request_json("/label", r##"["ch", 7, "raw"]"##),
    );
    assert_eq!(body_text(&resp), r#""ch-7-raw""#);
}

#[test]
fn multi_argument_methods_accept_an_object_keyed_by_parameter_name() {
    let router = router_with(Device::with_id("sensor-42"));

    let resp = call(
        &router,
        "/scale",
        &request_json("/scale", r##"{"offset": 1.5, "factor": 3.0}"##),
    );
    assert_eq!(body_text(&resp), "7.5");

    let resp = call(
        &router,
        "/label",
        &request_json(
            "/label",
            r##"{"prefix": "ch", "index": 7, "suffix": "raw"}"##,
        ),
    );
    assert_eq!(body_text(&resp), r#""ch-7-raw""#);
}

#[test]
fn a_multi_argument_body_of_the_wrong_shape_is_an_error() {
    let router = router_with(Device::with_id("sensor-42"));

    let short = call(&router, "/scale", &request_json("/scale", r##"[3.0]"##));
    assert_eq!(short.header.ec, ErrorCode::InvalidBody as u32);
    assert!(
        String::from_utf8_lossy(&short.body).contains("expected 2 arguments, got 1"),
        "unexpected error: {}",
        String::from_utf8_lossy(&short.body)
    );

    let scalar = call(&router, "/scale", &request_json("/scale", r##"3.0"##));
    assert_eq!(scalar.header.ec, ErrorCode::InvalidBody as u32);
    assert!(
        String::from_utf8_lossy(&scalar.body).contains("factor, offset"),
        "unexpected error: {}",
        String::from_utf8_lossy(&scalar.body)
    );

    let wrong_type = call(
        &router,
        "/scale",
        &request_json("/scale", r##"{"factor": "three", "offset": 1.5}"##),
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
    assert_eq!(body_text(&ok), "null");
    assert!(handle.lock().unwrap().armed);

    let ok = call(&router, "/checked_id", &request_empty("/checked_id"));
    assert_eq!(ok.header.ec, ErrorCode::Ok as u32);
    assert_eq!(body_text(&ok), r#""sensor-42""#);

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

    let listing = call(&router, "", &request_empty(""));
    let body = body_text(&listing);

    for expected in [
        r#""id":"sensor-42""#,
        r#""greet":"fn(&self) -> String""#,
        r#""scale":"fn(&self, factor: f64, offset: f64) -> f64""#,
        r#""set_armed":"fn(&mut self, armed: bool) -> ()""#,
        r#""arm":"fn(&mut self) -> Result<(), DeviceError>""#,
        r#""identity":"fn(&self) -> String""#,
        // A typed field is a plain JSON array inside the listing: the frame is
        // already JSON, so the BEVE encoding is reachable only per-field.
        r#""samples":[0,0,0,0,0,0,0,0]"#,
    ] {
        assert!(body.contains(expected), "missing {expected} in {body}");
    }

    for absent in ["id_string", "internal_checksum", "with_id"] {
        assert!(!body.contains(absent), "unexpected {absent} in {body}");
    }
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
        &request_json("/trace", r##"[9.0, 8.0]"##),
    );
    assert_eq!(body_text(&resp), "null");
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
    assert_eq!(body_text(&resp), r#""gain=2""#);

    let resp = call(
        &router,
        "/channel/blend",
        &request_json("/channel/blend", r##"[1.0, 3.0]"##),
    );
    assert_eq!(body_text(&resp), "8");

    let listing = call(&router, "", &request_empty(""));
    let listing = body_text(&listing);
    assert!(listing.contains(r#""gain":2"#), "{listing}");
    assert!(
        listing.contains(r#""describe":"fn(&self) -> String""#),
        "{listing}"
    );

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

    let resp = call(&router, "/listed", &request_json("/listed", r##"[1, 2]"##));
    assert_eq!(body_text(&resp), "13");

    let resp = call(
        &router,
        "/listed",
        &request_json("/listed", r##"{"left": 1, "right": 2}"##),
    );
    assert_eq!(body_text(&resp), "13");
}

#[test]
fn a_listed_method_and_an_impl_block_coexist() {
    let (router, handle) = Router::new().with_struct("", MixedSurface::default());
    handle.lock().unwrap().base = 10;

    let resp = call(
        &router,
        "/via_impl_block",
        &request_json("/via_impl_block", r##"[1, 2, 3]"##),
    );
    assert_eq!(body_text(&resp), "16");

    let listing = call(&router, "", &request_empty(""));
    let listing = body_text(&listing);
    assert!(listing.contains(r#""base":10"#), "{listing}");
    assert!(
        listing.contains(r#""listed":"fn(&self, left: i32, right: i32) -> i32""#),
        "{listing}"
    );
    assert!(
        listing.contains(r#""via_impl_block":"fn(&self, a: i32, b: i32, c: i32) -> i32""#),
        "{listing}"
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
    // A BEVE request is answered in BEVE, so the response is read through
    // `decode_body`, which follows the header rather than assuming a format.
    assert_eq!(resp.header.body_format, BodyFormat::Beve as u16);
    assert_eq!(resp.decode_body::<f64>().expect("body"), 7.5);
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
structio::object!(Inner {
    scale,
    window,
    serial
});

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
structio::object!(Outer {
    name,
    inner,
    hidden
});

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

/// Every arm the derive generates, checked against the bytes it writes.
///
/// This used to be an agreement test: `repe_handle` built a `Value` and
/// `repe_handle_into` wrote bytes, and the two had to answer identically for
/// every shape. There is one path now — a response is written into the caller's
/// buffer as it is produced — so what is left to pin is the arms themselves.
#[test]
fn every_dispatch_arm_writes_what_it_should() {
    use repe::structs::{RepeStruct, RequestBody, ResponseBody, StructError};

    /// `Ok` with the exact JSON text expected, or `Err` with the error's
    /// `to_string()`.
    type Expected = Result<&'static str, &'static str>;

    let cases: [(&[&str], Option<&str>, Expected); 12] = [
        // Whole object: a listing, in declaration order.
        (
            &[],
            None,
            Ok(
                r#"{"name":"rack","inner":{"scale":1.5,"window":[1,-2,3,-4],"serial":"SN-1","doubled":"fn(&self) -> f64"}}"#,
            ),
        ),
        (&["name"], None, Ok(r#""rack""#)),
        (&["name"], Some(r#""renamed""#), Ok("null")),
        (
            &["inner"],
            None,
            Ok(
                r#"{"scale":1.5,"window":[1,-2,3,-4],"serial":"SN-1","doubled":"fn(&self) -> f64"}"#,
            ),
        ),
        (
            &["inner"],
            Some(r#"{"scale": 4.0, "window": [0, 0, 0, 0], "serial": "SN-2"}"#),
            Ok("null"),
        ),
        (&["inner", "scale"], None, Ok("1.5")),
        (&["inner", "scale"], Some("9.5"), Ok("null")),
        (&["inner", "doubled"], None, Ok("3")),
        // A read-only field refuses a write rather than silently ignoring it.
        (
            &["inner", "serial"],
            Some(r#""nope""#),
            Err("body not allowed for `/inner/serial`"),
        ),
        (&["hidden"], None, Err("invalid path `/hidden`")),
        (
            &["name", "extra"],
            None,
            Err("unexpected additional path segments at `/name/extra`"),
        ),
        (&["nope"], None, Err("invalid path `/nope`")),
    ];

    for (segments, body, expected) in cases {
        let mut subject = outer();
        let body = body.map(|text| RequestBody::new(text.as_bytes(), BodyFormat::Json));

        let mut buf = Vec::new();
        let mut out = ResponseBody::new(&mut buf);
        let result = subject.repe_handle_into(segments, body, &mut out);

        match (result, expected) {
            (Ok(()), Ok(want)) => {
                let got = std::str::from_utf8(&buf).expect("a JSON response is UTF-8");
                assert_eq!(got, want, "payload diverged at {segments:?}");
            }
            (Err(err), Err(want)) => {
                assert_eq!(err.to_string(), want, "error text diverged at {segments:?}");
            }
            (got, want) => panic!("outcome diverged at {segments:?}: {got:?} vs {want:?}"),
        }
    }

    // A write that succeeded actually landed.
    let mut subject = outer();
    let body = RequestBody::new(br#"9.5"#, BodyFormat::Json);
    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    subject
        .repe_handle_into(&["inner", "scale"], Some(body), &mut out)
        .expect("a nested leaf write");
    assert_eq!(subject.inner.scale, 9.5);

    // And the error type is the one the trait declares, not a stringly one.
    let mut subject = outer();
    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    assert!(matches!(
        subject.repe_handle_into(&["nope"], None, &mut out),
        Err(StructError::InvalidPath { .. })
    ));
}

/// A `#[repe(typed)]` field answers a *direct* read in BEVE and appears as a
/// plain JSON array inside a listing.
///
/// The two are not in tension: the listing's frame is already JSON, so there is
/// nowhere in it to put a BEVE body, while a direct read owns its whole frame
/// and can declare whichever format it likes.
#[test]
fn a_typed_field_reads_as_beve_directly_and_as_json_in_a_listing() {
    use repe::structs::{RepeStruct, ResponseBody};

    let mut device = outer();
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
    let listing = call(
        &Router::new().with_struct("", outer()).0,
        "",
        &request_empty(""),
    );
    let listing = body_text(&listing);
    assert!(listing.contains(r#""window":[1,-2,3,-4]"#), "{listing}");
}

/// A `Result` return spelled by name becomes a REPE error response.
///
/// The counterpart case — a `Result` behind an alias that is not *named*
/// `Result` — cannot be recognized by a macro, so the derive treats it as an
/// ordinary return type. Under serde that shipped a surprising `{"Err": ".."}`
/// body, and this test pinned it as a decision on record. It is now a *compile*
/// error instead: `Result<T, E>` has no structio declaration, so a method
/// returning one through an alias does not build, and the diagnostic names the
/// missing declaration. Better caught there than on the wire, and there is no
/// longer a runtime behavior to assert. See `docs/server.md`.
#[test]
fn a_result_return_becomes_an_error_response() {
    #[derive(Default, repe::RepeStruct)]
    #[repe(methods)]
    struct Fallible {
        v: i32,
    }
    structio::object!(Fallible { v });

    #[repe::methods]
    impl Fallible {
        fn spelled(&self) -> Result<i32, String> {
            Err("spelled".into())
        }
        fn succeeds(&self) -> Result<i32, String> {
            Ok(7)
        }
    }

    let router = Router::new().with_struct("", Fallible::default()).0;

    let failed = call(&router, "/spelled", &request_empty("/spelled"));
    assert_eq!(failed.header.ec, ErrorCode::ParseError as u32);
    assert!(
        String::from_utf8_lossy(&failed.body).contains("spelled"),
        "the error message should carry the handler's own text"
    );

    // And the `Ok` arm is the plain value, with no wrapper around it.
    let ok = call(&router, "/succeeds", &request_empty("/succeeds"));
    assert_eq!(ok.header.ec, ErrorCode::Ok as u32);
    assert_eq!(body_text(&ok), "7");
}
