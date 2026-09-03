//! The multi-argument wire contract: what `MethodArgs` accepts and refuses.
//!
//! These pin behaviour a client depends on, so they are written against bytes
//! rather than against a helper that would encode the same assumption twice.

use repe_core::constants::BodyFormat;
use repe_core::{MethodArgs, RequestBody, StructError};

const NAMES: &[&str] = &["a", "b"];

fn split_json(body: &str) -> Result<(i64, Option<i64>), StructError> {
    let body = RequestBody::new(body.as_bytes(), BodyFormat::Json);
    let mut args = MethodArgs::new("/m", NAMES, body)?;
    Ok((args.next_arg::<i64>()?, args.next_arg::<Option<i64>>()?))
}

fn split_beve(bytes: &[u8]) -> Result<(f64, Option<f64>), StructError> {
    let body = RequestBody::new(bytes, BodyFormat::Beve);
    let mut args = MethodArgs::new("/m", NAMES, body)?;
    Ok((args.next_arg::<f64>()?, args.next_arg::<Option<f64>>()?))
}

#[test]
fn json_array_form_is_positional() {
    assert_eq!(split_json("[1, 2]").unwrap(), (1, Some(2)));
}

#[test]
fn json_object_form_is_by_name_and_order_free() {
    assert_eq!(split_json(r#"{"b": 2, "a": 1}"#).unwrap(), (1, Some(2)));
}

#[test]
fn an_omitted_optional_key_is_none() {
    assert_eq!(split_json(r#"{"a": 1}"#).unwrap(), (1, None));
}

#[test]
fn an_unknown_key_is_ignored() {
    assert_eq!(
        split_json(r#"{"a": 1, "z": 9, "b": 2}"#).unwrap(),
        (1, Some(2))
    );
}

#[test]
fn a_repeated_key_takes_its_last_value() {
    // `serde_json::Map` did this by overwriting on insert; the span walk does it
    // by overwriting the slot. Pinned because it is wire-visible either way.
    assert_eq!(
        split_json(r#"{"a": 1, "a": 9, "b": 2}"#).unwrap(),
        (9, Some(2))
    );
}

#[test]
fn the_wrong_arity_is_an_arguments_error() {
    let err = split_json("[1]").unwrap_err();
    assert!(
        matches!(&err, StructError::Arguments { message, .. } if message.contains("expected 2")),
        "got {err}"
    );
}

#[test]
fn a_scalar_body_names_both_accepted_shapes() {
    let err = split_json("7").unwrap_err();
    let StructError::Arguments { message, .. } = &err else {
        panic!("got {err}")
    };
    assert!(
        message.contains("array") && message.contains("[a, b]"),
        "got {message}"
    );
}

#[test]
fn a_malformed_element_reports_the_parse_failure_not_the_shape() {
    // The array form is decided from the opening byte, so a body that plainly
    // *is* an array reports where it went wrong rather than being re-tried as
    // an object and reported as neither.
    let err = split_json(r#"[1, {"#).unwrap_err();
    assert!(matches!(err, StructError::Decode { .. }), "got {err}");
}

#[test]
fn trailing_content_after_the_argument_list_is_rejected() {
    assert!(matches!(
        split_json("[1, 2] junk").unwrap_err(),
        StructError::Decode { .. }
    ));
    assert!(matches!(
        split_json(r#"{"a":1,"b":2}}}"#).unwrap_err(),
        StructError::Decode { .. }
    ));
}

#[test]
fn beve_generic_array_form_is_positional() {
    let mut body = Vec::new();
    structio::beve::append(&(1.5f64, 2.5f64), &mut body);
    assert_eq!(split_beve(&body).unwrap(), (1.5, Some(2.5)));
}

#[test]
fn beve_object_form_is_by_name() {
    let mut body = Vec::new();
    structio::beve::Writer::<structio::Standard>::appending(std::mem::take(&mut body));
    structio::beve::append(
        &[("b", 2.5f64), ("a", 1.5f64)]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        &mut body,
    );
    assert_eq!(split_beve(&body).unwrap(), (1.5, Some(2.5)));
}

#[test]
fn a_beve_typed_array_is_accepted() {
    // Its elements carry no headers of their own, so they cannot be sliced out
    // as spans. They are pulled as documents instead, with the header the
    // reader supplies. A client with N doubles has no reason to encode them
    // generically, and Glaze produces a typed array for a homogeneous list.
    let mut body = Vec::new();
    structio::beve::append(&vec![1.5f64, 2.5f64], &mut body);
    assert_eq!(split_beve(&body).unwrap(), (1.5, Some(2.5)));
}

#[test]
fn a_beve_typed_bool_array_is_accepted() {
    // The shape whose spans were empty *and* not monotonic, so a span walk over
    // it could mis-pair rather than fail. Pinned because it is the one that
    // would fail silently.
    let mut body = Vec::new();
    structio::beve::append(&vec![true, false], &mut body);
    let body = RequestBody::new(&body, BodyFormat::Beve);
    let mut args = MethodArgs::new("/m", NAMES, body).unwrap();
    assert!(args.next_arg::<bool>().unwrap());
    assert!(!args.next_arg::<bool>().unwrap());
}

#[test]
fn a_beve_complex_array_is_accepted() {
    // Reaches `read_seq` through `complex_run`, which installs a synthetic
    // element header exactly as a typed array installs a real one.
    let mut body = Vec::new();
    structio::beve::append(
        &vec![
            structio::Complex {
                re: 1.0f64,
                im: 2.0,
            },
            structio::Complex {
                re: 3.0f64,
                im: 4.0,
            },
        ],
        &mut body,
    );
    let body = RequestBody::new(&body, BodyFormat::Beve);
    let mut args = MethodArgs::new("/m", NAMES, body).unwrap();
    assert_eq!(
        args.next_arg::<structio::Complex<f64>>().unwrap(),
        structio::Complex { re: 1.0, im: 2.0 }
    );
    assert_eq!(
        args.next_arg::<structio::Complex<f64>>().unwrap(),
        structio::Complex { re: 3.0, im: 4.0 }
    );
}

#[test]
fn a_typed_array_of_the_wrong_length_still_fails_arity_first() {
    // Arity is settled before any argument is decoded, whichever encoding the
    // list arrived in, so a wrong-length call fails the same way for all of them.
    let mut body = Vec::new();
    structio::beve::append(&vec![1.5f64, 2.5, 3.5], &mut body);
    let err = split_beve(&body).unwrap_err();
    assert!(
        matches!(&err, StructError::Arguments { message, .. } if message.contains("expected 2")),
        "got {err}"
    );
}
