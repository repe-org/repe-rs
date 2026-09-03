//! What this crate exists for: a struct declared, derived, and dispatched with
//! `repe` nowhere in the dependency graph.
//!
//! This file is the proof rather than a description of one. `repe-core` and
//! `structio` are the only dependencies in scope, so anything the derive emits
//! that lives on the server side of the split would fail to resolve here —
//! which is exactly the failure the split was made to rule out. A pure-logic
//! crate can now declare its served surface beside the types it already owns,
//! and the crate that runs the RPC picks it up through `repe`'s re-export at
//! the same paths.

use repe_core::structs::{RequestBody, ResponseBody, StructError};
// `RepeStruct` is two things in two namespaces: the derive macro and the trait
// whose methods are called below. One import brings both.
use repe_core::{BodyFormat, ErrorCode, RepeStruct};

/// The shape a pure crate has: data, and a computed endpoint beside it. No I/O,
/// no lock, no transport.
#[derive(Default, RepeStruct)]
#[repe(methods)]
struct Sample {
    count: u64,
    label: String,
    // Read as a BEVE typed array. This used to sit behind a `typed` feature,
    // because the encoder arrived with `beve` and six transitive packages
    // behind it; structio has no dependencies, so there is nothing left to
    // gate.
    #[repe(typed)]
    values: Vec<f64>,
}

// The encoding, declared separately from the endpoints. `#[derive(RepeStruct)]`
// says which paths are served; `structio::object!` says what the type looks like
// on the wire. A served type needs both.
structio::object!(Sample {
    count,
    label,
    values
});

// Spelled for the crate that is actually a dependency here. Under `repe` the
// same attribute is `#[repe::methods]`; it is one macro either way.
#[repe_core::methods]
impl Sample {
    #[repe(get = "mean")]
    fn mean(&self) -> f64 {
        if self.values.is_empty() {
            0.0
        } else {
            self.values.iter().sum::<f64>() / self.values.len() as f64
        }
    }

    fn describe(&self) -> String {
        format!("sample {label}", label = self.label)
    }
}

fn sample() -> Sample {
    Sample {
        count: 2,
        label: String::from("alpha"),
        values: vec![1.0, 2.0],
    }
}

/// A JSON request body, as it arrives: bytes plus the format the header declared.
fn json(body: &str) -> RequestBody<'_> {
    RequestBody::new(body.as_bytes(), BodyFormat::Json)
}

/// Dispatch one request the way a router would, and hand back the body bytes and
/// the format the write settled on.
fn dispatch(
    value: &mut Sample,
    segments: &[&str],
    body: Option<RequestBody<'_>>,
) -> Result<(Vec<u8>, BodyFormat), StructError> {
    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    value.repe_handle_into(segments, body, &mut out)?;
    let format = out.format();
    Ok((buf, format))
}

/// The same through the shared borrow, which is the other half of the trait a
/// server calls.
fn dispatch_shared(
    value: &Sample,
    segments: &[&str],
    body: Option<RequestBody<'_>>,
) -> Option<Result<Vec<u8>, StructError>> {
    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    let outcome = value.repe_shared_into(segments, body, &mut out)?;
    Some(outcome.map(|()| buf))
}

#[test]
fn a_derived_struct_dispatches_with_only_the_core_crate() {
    let mut value = sample();

    let (body, format) = dispatch(&mut value, &["label"], None).expect("a leaf read");
    assert_eq!(body, br#""alpha""#);
    assert_eq!(format, BodyFormat::Json);

    let (body, _) = dispatch(&mut value, &[], None).expect("a whole-object read");
    // Compared as bytes rather than as a parsed tree: key order is part of the
    // contract now that every listing streams, and a tree comparison would not
    // see it.
    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        r#"{"count":2,"label":"alpha","values":[1,2],"describe":"fn(&self) -> String","mean":1.5}"#
    );

    dispatch(&mut value, &["label"], Some(json(r#""beta""#))).expect("a leaf write");
    assert_eq!(value.label, "beta");
}

#[test]
fn the_typed_encoding_is_reachable_from_the_core_crate() {
    // `ResponseBody::write_typed_slice` is the bulk numeric path, and
    // `#[repe(typed)]` is how a derived field reaches it. Losing it would be
    // silent: the body would still be a correct JSON array.
    let mut value = sample();
    let (body, format) = dispatch(&mut value, &["values"], None).expect("a typed read");
    assert_eq!(format, BodyFormat::Beve);
    assert_eq!(
        structio::from_beve::<Vec<f64>>(&body).unwrap(),
        vec![1.0, 2.0]
    );
}

#[test]
fn a_beve_request_is_answered_in_beve() {
    // The frame header picks the format per request, so a response is encoded in
    // whichever one the request asked for rather than always in JSON.
    let value = sample();
    let mut buf = Vec::new();
    let mut out = ResponseBody::with_format(&mut buf, BodyFormat::Beve);
    value
        .repe_shared_into(&["label"], None, &mut out)
        .expect("a `&self` field read is servable shared")
        .expect("and succeeds");
    assert_eq!(out.format(), BodyFormat::Beve);
    assert_eq!(structio::from_beve::<String>(&buf).unwrap(), "alpha");
}

#[test]
fn a_beve_request_gets_a_beve_whole_object_listing() {
    // The listing is where the member count matters: BEVE puts it in the
    // object's header, before any member, so this only works because the derive
    // knows the count when it generates the walk.
    let value = sample();
    let mut buf = Vec::new();
    let mut out = ResponseBody::with_format(&mut buf, BodyFormat::Beve);
    value
        .repe_shared_into(&[], None, &mut out)
        .expect("a whole-object read is servable shared")
        .expect("and succeeds");
    assert_eq!(out.format(), BodyFormat::Beve);

    #[derive(Debug, Default, PartialEq)]
    struct Listing {
        count: u64,
        label: String,
        values: Vec<f64>,
        describe: String,
        mean: f64,
    }
    structio::object!(Listing {
        count,
        label,
        values,
        describe,
        mean
    });

    assert_eq!(
        structio::from_beve::<Listing>(&buf).unwrap(),
        Listing {
            count: 2,
            label: String::from("alpha"),
            values: vec![1.0, 2.0],
            describe: String::from("fn(&self) -> String"),
            mean: 1.5,
        }
    );
}

#[test]
fn the_shared_borrow_path_is_generated_here_too() {
    let value = sample();
    let body = dispatch_shared(&value, &["mean"], None)
        .expect("a `&self` getter is servable shared")
        .expect("and succeeds");
    assert_eq!(body, b"1.5");

    assert!(
        dispatch_shared(&value, &["label"], Some(json(r#""beta""#))).is_none(),
        "a field write needs `&mut self`, so the shared borrow declines it"
    );
}

#[test]
fn the_error_type_and_its_codes_come_from_here() {
    let mut value = sample();
    let err = dispatch(&mut value, &["missing"], None).expect_err("an unknown path");
    assert_eq!(err.code(), ErrorCode::MethodNotFound);
}
