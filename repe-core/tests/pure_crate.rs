//! What this crate exists for: a struct declared, derived, and dispatched with
//! `repe` nowhere in the dependency graph.
//!
//! This file is the proof rather than a description of one. `repe-core` is the
//! only dependency in scope, so anything the derive emits that lives on the
//! server side of the split would fail to resolve here — which is exactly the
//! failure the split was made to rule out. A pure-logic crate can now declare
//! its served surface beside the types it already owns, and the crate that runs
//! the RPC picks it up through `repe`'s re-export at the same paths.

use repe_core::structs::{ResponseBody, StructError};
// `RepeStruct` is two things in two namespaces: the derive macro and the trait
// whose methods are called below. One import brings both.
use repe_core::{BodyFormat, ErrorCode, RepeStruct};
use serde::{Deserialize, Serialize};

/// The shape a pure crate has: data, and a computed endpoint beside it. No I/O,
/// no lock, no transport.
#[derive(Default, Serialize, Deserialize, RepeStruct)]
#[repe(methods)]
struct Sample {
    count: u64,
    label: String,
    #[repe(typed)]
    values: Vec<f64>,
}

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

/// Dispatch one request the way a router would, and hand back the body bytes and
/// the format the write settled on.
fn dispatch(
    value: &mut Sample,
    segments: &[&str],
    body: Option<serde_json::Value>,
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
    mut body: Option<serde_json::Value>,
) -> Option<Result<Vec<u8>, StructError>> {
    let mut buf = Vec::new();
    let mut out = ResponseBody::new(&mut buf);
    let outcome = value.repe_shared_into(segments, &mut body, &mut out)?;
    Some(outcome.map(|()| buf))
}

#[test]
fn a_derived_struct_dispatches_with_only_the_core_crate() {
    let mut value = sample();

    let (body, format) = dispatch(&mut value, &["label"], None).expect("a leaf read");
    assert_eq!(body, br#""alpha""#);
    assert_eq!(format, BodyFormat::Json);

    let (body, _) = dispatch(&mut value, &[], None).expect("a whole-object read");
    let listing: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
    assert_eq!(
        listing,
        serde_json::json!({
            "count": 2,
            "label": "alpha",
            "values": [1.0, 2.0],
            "describe": "fn(&self) -> String",
            "mean": 1.5,
        })
    );

    dispatch(&mut value, &["label"], Some(serde_json::json!("beta"))).expect("a leaf write");
    assert_eq!(value.label, "beta");
}

#[test]
fn the_typed_encoding_is_reachable_from_the_core_crate() {
    // `ResponseBody::write_typed_slice` is the one place the core crate touches
    // beve, and `#[repe(typed)]` is how a derived field reaches it. Losing it in
    // the split would be silent: the body would still be a correct JSON array.
    let mut value = sample();
    let (body, format) = dispatch(&mut value, &["values"], None).expect("a typed read");
    assert_eq!(format, BodyFormat::Beve);
    assert_eq!(beve::from_slice::<Vec<f64>>(&body).unwrap(), vec![1.0, 2.0]);
}

#[test]
fn the_shared_borrow_path_is_generated_here_too() {
    let value = sample();
    let body = dispatch_shared(&value, &["mean"], None)
        .expect("a `&self` getter is servable shared")
        .expect("and succeeds");
    assert_eq!(body, b"1.5");

    assert!(
        dispatch_shared(&value, &["label"], Some(serde_json::json!("beta"))).is_none(),
        "a field write needs `&mut self`, so the shared borrow declines it"
    );
}

#[test]
fn the_error_type_and_its_codes_come_from_here() {
    let mut value = sample();
    let err = dispatch(&mut value, &["missing"], None).expect_err("an unknown path");
    assert_eq!(err.code(), ErrorCode::MethodNotFound);
}
