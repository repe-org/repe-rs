//! An untrusted body declares its own nesting, so the depth a decoder recurses
//! to is chosen by the sender rather than by the destination type.
//!
//! Before `beve` 9 that had no ceiling, and it was not an ordinary parse
//! failure: a Rust stack overflow *aborts* rather than unwinding, so no
//! `Result` carried it and the per-connection `catch_unwind` in the servers
//! could not contain it. One anonymous request took down every other connection
//! the process was serving, and no body-size limit a caller would plausibly set
//! was small enough to help. `beve` 9 bounds nesting at
//! `beve::MAX_RECURSION_DEPTH`, so this is now a refusal like any other.
//!
//! These pin both sides of that ceiling, so a beve bump that reintroduced the
//! hazard — or narrowed the ceiling onto ordinary documents — fails here rather
//! than in a deployment.

use repe::{BodyFormat, ErrorCode, Message, QueryFormat, Router};

/// A BEVE document nested `depth` generic arrays deep, innermost empty, built by
/// hand: encoding this through serde would recurse in the *serializer* to
/// construct the very shape under test, and the value would recurse again on
/// drop. `05` opens a generic array, `04` is a one-element size tag, `00` is an
/// empty one.
fn nested_beve(depth: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(depth * 2);
    for _ in 0..depth - 1 {
        bytes.extend_from_slice(&[0x05, 0x04]);
    }
    bytes.extend_from_slice(&[0x05, 0x00]);
    bytes
}

fn beve_request(path: &str, body: Vec<u8>) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_format(BodyFormat::Beve)
        .body_bytes(body)
        .build()
        .to_vec()
}

fn router() -> Router {
    Router::new().with_json("/echo", Ok)
}

fn call(depth: usize) -> Message {
    let response = router()
        .call(&beve_request("/echo", nested_beve(depth)))
        .expect("a non-notify request produces a response");
    Message::from_slice(&response).unwrap()
}

#[test]
fn a_body_nested_past_the_ceiling_is_refused_rather_than_aborting() {
    // 40 KB, the size that reproduced the abort against a gateway. Reaching the
    // assertion at all is most of what this test proves.
    assert_eq!(call(20_000).error_code(), Some(ErrorCode::ParseError));
    // And immediately past it, so the boundary is pinned rather than the
    // extreme: a ceiling raised silently would still pass the case above.
    assert_eq!(
        call(beve::MAX_RECURSION_DEPTH + 1).error_code(),
        Some(ErrorCode::ParseError)
    );
}

#[test]
fn a_body_at_the_ceiling_still_decodes() {
    // The ceiling has to admit what it advertises, or the fix is a denial of
    // service of its own.
    let depth = beve::MAX_RECURSION_DEPTH;
    let message = call(depth);
    assert_eq!(message.error_code(), Some(ErrorCode::Ok));
    // The handler echoes what it decoded, and answers a JSON body, so this is
    // the same nesting rendered as JSON — proof the body was decoded rather
    // than merely accepted.
    let echoed = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    assert_eq!(message.body, echoed.as_bytes());
}
