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

#[derive(Debug)]
struct RangeError(&'static str);

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "out of range: {}", self.0)
    }
}

/// The backing store. `used` is what the object holds; `used_percent` is what a
/// client wants to talk about, and there is no field for it.
#[derive(Debug, Default, repe::RepeStruct)]
#[repe(methods)]
struct Budget {
    used: u32,
    total: f64,
    tier: u32,
    /// Backs `/reads`, and is published in its own right — the two names have
    /// to differ, and the derive refuses them if they do not.
    read_count: u32,
}
structio::object!(Budget {
    used,
    total,
    tier,
    read_count
});

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
/// order a listing emits becomes assertable rather than normalized away.
fn body_text(resp: &Message) -> &str {
    std::str::from_utf8(&resp.body).expect("response body should be UTF-8 JSON")
}

#[test]
fn a_bodiless_request_reads_through_the_getter() {
    let router = Router::new().with_struct("", budget()).0;
    let resp = call(&router, "/used_percent", &request_empty("/used_percent"));
    assert_eq!(body_text(&resp), "12.5");
}

#[test]
fn a_request_with_a_body_writes_through_the_setter() {
    let (router, handle) = Router::new().with_struct("", budget());

    let resp = call(
        &router,
        "/used_percent",
        &request_json("/used_percent", r##"25.0"##),
    );
    assert_eq!(
        body_text(&resp),
        "null",
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
    assert_eq!(body_text(&resp), r#""1.4.2""#);

    // The same refusal a `#[repe(readonly)]` field gives: no setter is declared,
    // so there is nothing extra to say.
    let resp = call(&router, "/version", &request_json("/version", r##""2""##));
    assert_eq!(resp.header.ec, ErrorCode::InvalidBody as u32);
}

#[test]
fn the_typed_numeric_path_composes_with_an_accessor() {
    let router = Router::new().with_struct("", budget()).0;
    let resp = call(&router, "/weights", &request_empty("/weights"));
    assert_eq!(resp.header.body_format, BodyFormat::Beve as u16);
    assert_eq!(
        structio::from_beve::<Vec<f64>>(&resp.body).unwrap(),
        vec![1.0, 0.5, 0.25, 0.125]
    );
}

#[test]
fn either_half_may_fail() {
    let (router, handle) = Router::new().with_struct("", budget());

    let resp = call(&router, "/offset", &request_empty("/offset"));
    assert_eq!(body_text(&resp), "2");

    let resp = call(&router, "/offset", &request_json("/offset", r##"64"##));
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
        &request_json("/used_percent", r##""fast""##),
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
    let listing = call(&router, "", &request_empty(""));
    let listing = body_text(&listing);

    // Fields, as always.
    assert!(listing.contains(r#""used":512"#), "{listing}");

    // Field-shaped endpoints list like fields: by value, not by signature.
    assert!(listing.contains(r#""used_percent":12.5"#), "{listing}");
    assert!(listing.contains(r#""version":"1.4.2""#), "{listing}");
    assert!(listing.contains(r#""offset":2"#), "{listing}");
    assert!(
        listing.contains(r#""weights":[1,0.5,0.25,0.125]"#),
        "the enclosing object is JSON, so a typed accessor lists as a JSON array: {listing}"
    );

    // An ordinary method still publishes its signature.
    assert!(
        listing.contains(r#""reset":"fn(&mut self) -> ()""#),
        "{listing}"
    );
}

#[test]
fn the_two_halves_of_a_pair_need_not_be_adjacent() {
    // `set_offset` is declared above `reads` and `offset` below it, so the pairing
    // is by endpoint name rather than by position in the block.
    let (router, handle) = Router::new().with_struct("", budget());
    call(&router, "/offset", &request_json("/offset", r##"6"##));
    assert_eq!(handle.lock().unwrap().tier, 3);
    assert_eq!(
        body_text(&call(&router, "/offset", &request_empty("/offset"))),
        "6"
    );
}

#[test]
fn a_getter_may_take_a_mut_receiver() {
    let (router, handle) = Router::new().with_struct("", budget());
    assert_eq!(
        body_text(&call(&router, "/reads", &request_empty("/reads"))),
        "1"
    );
    assert_eq!(
        body_text(&call(&router, "/reads", &request_empty("/reads"))),
        "2"
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

    #[derive(Debug, Default, repe::RepeStruct)]
    struct Plan {
        label: String,
        #[repe(nested)]
        budget: Budget,
    }
    structio::object!(Plan { label, budget });

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
            body_text(&call(
                &router,
                "/budget/used_percent",
                &request_empty("/budget/used_percent")
            )),
            "12.5"
        );

        call(
            &router,
            "/budget/used_percent",
            &request_json("/budget/used_percent", r##"25.0"##),
        );
        assert_eq!(handle.lock().unwrap().budget.used, 1024);

        // The child's own listing carries the accessor values.
        let child = call(&router, "/budget", &request_empty("/budget"));
        let child = body_text(&child);
        assert!(child.contains(r#""used_percent":25"#), "{child}");
        assert!(child.contains(r#""version":"1.4.2""#), "{child}");

        // And an error from inside the child names the full path.
        let resp = call(
            &router,
            "/budget/version",
            &request_json("/budget/version", r##""2.0""##),
        );
        assert_eq!(resp.header.ec, ErrorCode::InvalidBody as u32);
        assert!(
            String::from_utf8_lossy(&resp.body).contains("/budget/version"),
            "the child's error path is prefixed with the field it came from"
        );
    }
}

/// Every accessor arm the derive generates, checked against the bytes it writes.
///
/// This was an agreement test between `repe_handle` (which built a `Value`) and
/// `repe_handle_into` (which wrote bytes). There is one path now, so what is
/// left to pin is the arms themselves.
#[test]
fn every_accessor_arm_writes_what_it_should() {
    use repe::structs::RequestBody;

    /// `Ok` with the exact JSON text expected, or `Err` with the error's
    /// `to_string()`.
    type Expected = Result<&'static str, &'static str>;

    let cases: [(&[&str], Option<&str>, Expected); 10] = [
        // A field-shaped endpoint lists by value, not by signature.
        (
            &[],
            None,
            Ok(concat!(
                r#"{"used":512,"total":4096,"tier":1,"read_count":0,"#,
                r#""reset":"fn(&mut self) -> ()","used_percent":12.5,"version":"1.4.2","#,
                r#""weights":[1,0.5,0.25,0.125],"reads":1,"offset":2}"#
            )),
        ),
        (&["used_percent"], None, Ok("12.5")),
        (&["used_percent"], Some("25.0"), Ok("null")),
        // A setter's parameter type is enforced: a string is not a percentage.
        (
            &["used_percent"],
            Some(r#""fast""#),
            Err("could not decode body for `/used_percent`: expected a number at byte 0"),
        ),
        (&["version"], None, Ok(r#""1.4.2""#)),
        (
            &["version"],
            Some(r#""2.0""#),
            Err("body not allowed for `/version`"),
        ),
        (&["offset"], None, Ok("2")),
        // A fallible setter's own refusal reaches the caller intact.
        (
            &["offset"],
            Some("64"),
            Err("method `/offset` failed: out of range: offset must be within ±8"),
        ),
        (
            &["used_percent", "extra"],
            None,
            Err("unexpected additional path segments at `/used_percent/extra`"),
        ),
        (&["reset"], None, Ok("null")),
    ];

    for (segments, body, expected) in cases {
        let mut subject = budget();
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
}

/// A `#[repe(typed)]` accessor read on its own carries a BEVE body, the same as
/// a `#[repe(typed)]` field. Inside a listing it is a JSON array, because the
/// enclosing frame is already JSON.
#[test]
fn a_typed_accessor_reads_as_beve_directly() {
    let mut budget = budget();
    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    budget
        .repe_handle_into(&["weights"], None, &mut out)
        .unwrap();
    assert_eq!(out.format(), BodyFormat::Beve);
    assert_eq!(
        buf,
        Message::builder()
            .body_typed_slice(&[1.0f64, 0.5, 0.25, 0.125])
            .build()
            .body
    );
}
