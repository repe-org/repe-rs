//! `#[repe(get = "...")]` / `#[repe(set = "...")]`: **field-shaped endpoints**
//! backed by a getter/setter pair rather than by storage.
//!
//! The endpoint reads and writes like a field — a bodiless request reads it, a
//! request with a body writes it, and the whole-struct listing shows its value —
//! but the value is computed. That is the shape a derived or unit-converted
//! value needs, where there is no field to point at.

#![cfg(not(target_arch = "wasm32"))]

use repe::constants::{BodyFormat, ErrorCode, QueryFormat};
use repe::structs::{RepeStruct, ResponseBody};
use repe::{Message, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug)]
struct RangeError(&'static str);

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "out of range: {}", self.0)
    }
}

/// The backing store. `used` is what the object holds; `used_percent` is what a
/// client wants to talk about, and there is no field for it.
#[derive(Debug, Default, Serialize, Deserialize, repe::RepeStruct)]
#[repe(methods)]
struct Budget {
    used: u32,
    total: f64,
    tier: u32,
    /// Backs `/reads`, and is published in its own right — the two names have
    /// to differ, and the derive refuses them if they do not.
    read_count: u32,
}

#[repe::methods]
impl Budget {
    /// The read half of `/used_percent`.
    #[repe(get = "used_percent")]
    fn used_percent(&self) -> f64 {
        self.used as f64 * 100.0 / self.total
    }

    /// The write half, converting back into the stored units.
    #[repe(set = "used_percent")]
    fn set_used_percent(&mut self, percent: f64) {
        self.used = (percent * self.total / 100.0).round() as u32;
    }

    /// A getter with no setter: read-only, with nothing extra said.
    #[repe(get = "version")]
    fn version(&self) -> &'static str {
        "1.4.2"
    }

    /// The numeric bulk path composes, exactly as it does on a field.
    #[repe(get = "weights", typed)]
    fn weights(&self) -> Vec<f64> {
        vec![1.0, 0.5, 0.25, 0.125]
    }

    /// The write half of `/offset`. Deliberately *not* adjacent to its read half
    /// below: the two are paired by endpoint name, not by position.
    #[repe(set = "offset")]
    fn set_offset(&mut self, offset: i32) -> Result<(), RangeError> {
        if !(-8..=8).contains(&offset) {
            return Err(RangeError("offset must be within ±8"));
        }
        self.tier = offset.unsigned_abs() / 2;
        Ok(())
    }

    /// A getter that touches the backing store, so it takes `&mut self` and
    /// counts its own calls. Both receiver kinds are accepted.
    #[repe(get = "reads")]
    fn reads(&mut self) -> u32 {
        self.read_count += 1;
        self.read_count
    }

    /// A fallible pair: both halves may refuse.
    #[repe(get = "offset")]
    fn offset(&self) -> Result<i32, RangeError> {
        if self.tier > 3 {
            return Err(RangeError("tier is not selected"));
        }
        Ok(self.tier as i32 * 2)
    }

    /// An ordinary published method, unaffected by any of the above.
    fn reset(&mut self) {
        self.used = 0;
        self.tier = 0;
    }
}

fn budget() -> Budget {
    Budget {
        used: 512,
        total: 4096.0,
        tier: 1,
        read_count: 0,
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
        .unwrap()
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

#[test]
fn a_bodiless_request_reads_through_the_getter() {
    let router = Router::new().with_struct("", budget()).0;
    let resp = call(&router, "/used_percent", &request_empty("/used_percent"));
    assert_eq!(parse_body(&resp), json!(12.5));
}

#[test]
fn a_request_with_a_body_writes_through_the_setter() {
    let (router, handle) = Router::new().with_struct("", budget());

    let resp = call(
        &router,
        "/used_percent",
        &request_json("/used_percent", &json!(25.0)),
    );
    assert_eq!(
        parse_body(&resp),
        Value::Null,
        "a field write is acknowledged with null"
    );
    assert_eq!(
        handle.lock().unwrap().used,
        1024,
        "the setter converted back into the stored units"
    );
}

#[test]
fn a_getter_without_a_setter_is_read_only() {
    let router = Router::new().with_struct("", budget()).0;

    let resp = call(&router, "/version", &request_empty("/version"));
    assert_eq!(parse_body(&resp), json!("1.4.2"));

    // The same refusal a `#[repe(readonly)]` field gives: no setter is declared,
    // so there is nothing extra to say.
    let resp = call(&router, "/version", &request_json("/version", &json!("2")));
    assert_eq!(resp.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn the_typed_numeric_path_composes_with_an_accessor() {
    let router = Router::new().with_struct("", budget()).0;
    let resp = call(&router, "/weights", &request_empty("/weights"));
    assert_eq!(resp.header.body_format, BodyFormat::Beve as u16);
    assert_eq!(
        beve::from_slice::<Vec<f64>>(&resp.body).unwrap(),
        vec![1.0, 0.5, 0.25, 0.125]
    );
}

#[test]
fn either_half_may_fail() {
    let (router, handle) = Router::new().with_struct("", budget());

    let resp = call(&router, "/offset", &request_empty("/offset"));
    assert_eq!(parse_body(&resp), json!(2));

    let resp = call(&router, "/offset", &request_json("/offset", &json!(64)));
    assert_eq!(resp.header.ec, ErrorCode::ParseError as u32);
    assert!(String::from_utf8_lossy(&resp.body).contains("offset must be within"));
    assert_eq!(
        handle.lock().unwrap().tier,
        1,
        "a refused write leaves the object alone"
    );

    handle.lock().unwrap().tier = 9;
    let resp = call(&router, "/offset", &request_empty("/offset"));
    assert_eq!(resp.header.ec, ErrorCode::ParseError as u32);
}

#[test]
fn a_setter_argument_of_the_wrong_type_is_a_body_error() {
    let router = Router::new().with_struct("", budget()).0;
    let resp = call(
        &router,
        "/used_percent",
        &request_json("/used_percent", &json!("fast")),
    );
    assert_eq!(resp.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn an_accessor_endpoint_has_no_subpath() {
    let router = Router::new().with_struct("", budget()).0;
    let resp = call(
        &router,
        "/used_percent/extra",
        &request_empty("/used_percent/extra"),
    );
    assert_eq!(resp.header.ec, ErrorCode::MethodNotFound as u32);
}

#[test]
fn the_whole_struct_listing_shows_accessor_values_and_method_signatures() {
    let router = Router::new().with_struct("", budget()).0;
    let listing = parse_body(&call(&router, "", &request_empty("")));

    // Fields, as always.
    assert_eq!(listing["used"], json!(512));

    // Field-shaped endpoints list like fields: by value, not by signature.
    assert_eq!(listing["used_percent"], json!(12.5));
    assert_eq!(listing["version"], json!("1.4.2"));
    assert_eq!(listing["offset"], json!(2));
    assert_eq!(
        listing["weights"],
        json!([1.0, 0.5, 0.25, 0.125]),
        "the enclosing object is JSON, so a typed accessor lists as a JSON array"
    );

    // An ordinary method still publishes its signature.
    assert_eq!(listing["reset"], json!("fn(&mut self) -> ()"));
}

#[test]
fn the_two_halves_of_a_pair_need_not_be_adjacent() {
    // `set_offset` is declared above `reads` and `offset` below it, so the pairing
    // is by endpoint name rather than by position in the block.
    let (router, handle) = Router::new().with_struct("", budget());
    call(&router, "/offset", &request_json("/offset", &json!(6)));
    assert_eq!(handle.lock().unwrap().tier, 3);
    assert_eq!(
        parse_body(&call(&router, "/offset", &request_empty("/offset"))),
        json!(6)
    );
}

#[test]
fn a_getter_may_take_a_mut_receiver() {
    let (router, handle) = Router::new().with_struct("", budget());
    assert_eq!(
        parse_body(&call(&router, "/reads", &request_empty("/reads"))),
        json!(1)
    );
    assert_eq!(
        parse_body(&call(&router, "/reads", &request_empty("/reads"))),
        json!(2)
    );
    assert_eq!(handle.lock().unwrap().read_count, 2);
}

#[test]
fn a_failing_getter_fails_the_whole_object_read() {
    // The field analogy held to consistently: a field whose `Serialize` fails
    // takes the listing with it, and so does a getter that returns `Err`. Pinned
    // because it is the one place an accessor is *invoked* by a read of
    // something else, and because it is the reason a listed getter should report
    // a sentinel rather than an error.
    let (router, handle) = Router::new().with_struct("", budget());
    assert!(!call(&router, "", &request_empty("")).is_error());

    handle.lock().unwrap().tier = 9; // `/offset` now refuses
    let listing = call(&router, "", &request_empty(""));
    assert_eq!(listing.header.ec, ErrorCode::ParseError as u32);
    assert!(
        String::from_utf8_lossy(&listing.body).contains("tier is not selected"),
        "the listing carries the getter's own message"
    );
}

/// An accessor reached through a `#[repe(nested)]` parent: the path is prefixed,
/// the value lists inside the child's object, and an error path names the child.
mod nested {
    use super::*;

    #[derive(Debug, Default, Serialize, Deserialize, repe::RepeStruct)]
    struct Plan {
        label: String,
        #[repe(nested)]
        budget: Budget,
    }

    fn plan() -> Plan {
        Plan {
            label: "plan-1".into(),
            budget: budget(),
        }
    }

    #[test]
    fn an_accessor_is_reachable_through_a_nested_parent() {
        let (router, handle) = Router::new().with_struct("", plan());

        assert_eq!(
            parse_body(&call(
                &router,
                "/budget/used_percent",
                &request_empty("/budget/used_percent")
            )),
            json!(12.5)
        );

        call(
            &router,
            "/budget/used_percent",
            &request_json("/budget/used_percent", &json!(25.0)),
        );
        assert_eq!(handle.lock().unwrap().budget.used, 1024);

        // The child's own listing carries the accessor values.
        let child = parse_body(&call(&router, "/budget", &request_empty("/budget")));
        assert_eq!(child["used_percent"], json!(25.0));
        assert_eq!(child["version"], json!("1.4.2"));

        // And an error from inside the child names the full path.
        let resp = call(
            &router,
            "/budget/version",
            &request_json("/budget/version", &json!("2.0")),
        );
        assert_eq!(resp.header.ec, ErrorCode::InvalidBody as u32);
        assert!(
            String::from_utf8_lossy(&resp.body).contains("/budget/version"),
            "the child's error path is prefixed with the field it came from"
        );
    }
}

/// The two generated dispatch paths are built from one set of specs and must
/// agree on every accessor arm, exactly as they must on every field arm.
#[test]
fn the_value_and_encode_paths_agree() {
    let cases: [(&[&str], Option<Value>); 10] = [
        (&[], None),                              // whole object, listing
        (&["used_percent"], None),                // accessor read
        (&["used_percent"], Some(json!(25.0))),   // accessor write
        (&["used_percent"], Some(json!("fast"))), // write, wrong type
        (&["version"], None),                     // read-only accessor read
        (&["version"], Some(json!("2.0"))),       // read-only rejection
        (&["offset"], None),                      // fallible read
        (&["offset"], Some(json!(64))),           // fallible write, refused
        (&["used_percent", "extra"], None),       // InvalidSubpath
        (&["reset"], None),                       // ordinary method, alongside
    ];

    for (segments, body) in cases {
        let mut via_value = budget();
        let mut via_encode = budget();

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

        assert_eq!(
            serde_json::to_value(&via_value).unwrap(),
            serde_json::to_value(&via_encode).unwrap(),
            "struct state diverged at {segments:?}"
        );
    }
}

/// A `#[repe(typed)]` accessor read on its own carries a BEVE body; through the
/// `Value` path it is a JSON array, the same sanctioned divergence a typed field
/// has.
#[test]
fn a_typed_accessor_is_the_one_sanctioned_divergence() {
    let mut budget = budget();
    assert_eq!(
        budget.repe_handle(&["weights"], None).unwrap(),
        Some(json!([1.0, 0.5, 0.25, 0.125]))
    );

    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    budget
        .repe_handle_into(&["weights"], None, &mut out)
        .unwrap();
    assert_eq!(out.format(), BodyFormat::Beve);
}
