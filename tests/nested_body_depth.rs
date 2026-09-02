//! An untrusted body declares its own nesting, so the depth a decoder recurses
//! to is chosen by the sender rather than by the destination type.
//!
//! An unbounded decoder makes that a *stack overflow*, and a Rust stack
//! overflow **aborts** rather than unwinding: no `Result` carries it, and the
//! per-connection `catch_unwind` in the servers cannot contain it. One
//! anonymous request took down every other connection the process was serving,
//! and no body-size limit a caller would plausibly set was small enough to
//! help. structio bounds nesting at `structio::beve::reader::MAX_DEPTH`, so
//! this is a refusal like any other.
//!
//! These pin both sides of that ceiling, so a structio bump that reintroduced
//! the hazard — or narrowed the ceiling onto ordinary documents — fails here
//! rather than in a deployment.

use repe::{BodyFormat, ErrorCode, Message, QueryFormat, Router};

/// The reader's nesting ceiling, as a `usize` for the depth arithmetic here.
const MAX_DEPTH: usize = structio::beve::reader::MAX_DEPTH as usize;

/// A BEVE document nested `depth` generic arrays deep, innermost empty, built
/// by hand: encoding this through a writer would recurse to *construct* the
/// very shape under test, and the value would recurse again on drop. `05` opens
/// a generic array, `04` is a one-element size tag, `00` is an empty one.
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

/// The route the deep bodies are sent to.
///
/// Its parameter type is deliberately shallow. There is no type that can name
/// twenty thousand levels of nesting, and the point is not that the body
/// *decodes* — it is that the decoder refuses it and returns, rather than
/// overflowing the stack on the way down. A shallow declaration reaches the
/// ceiling the same way a deep one would, because the reader bounds its own
/// recursion before any type is consulted.
fn router() -> Router {
    Router::new().with_typed("/echo", |v: Vec<i64>| Ok(v))
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
    // assertion at all is most of what this test proves: an unbounded decoder
    // would have killed the process on the way in.
    assert_eq!(call(20_000).error_code(), Some(ErrorCode::ParseError));
    // And immediately past it, so the boundary is pinned rather than the
    // extreme: a ceiling raised silently would still pass the case above.
    assert_eq!(
        call(MAX_DEPTH + 1).error_code(),
        Some(ErrorCode::ParseError)
    );
}

#[test]
fn the_ceiling_is_where_it_says_it_is() {
    // Checked against the reader rather than through a route, because a route
    // names a type and no type can name this shape. `validate_beve` walks the
    // document's structure without one, which is exactly the question here:
    // where does the *decoder* stop, independent of what anyone declared.
    assert!(
        structio::validate_beve(&nested_beve(MAX_DEPTH)).is_ok(),
        "the ceiling has to admit what it advertises, or the fix is a denial of \
         service of its own"
    );

    let refused = structio::validate_beve(&nested_beve(MAX_DEPTH + 1))
        .expect_err("one level past the ceiling is refused");
    assert_eq!(
        refused.code,
        structio::ErrorCode::ExceededMaxDepth,
        "refused for the reason the ceiling exists, not incidentally"
    );
}

#[test]
fn an_ordinary_body_is_nowhere_near_the_ceiling() {
    // The other half of "narrowed onto ordinary documents": a body a real
    // caller would send decodes, and the route's declared type gets it.
    let request = beve_request("/echo", structio::to_beve(&vec![1i64, 2, 3]));
    let response = router()
        .call(&request)
        .expect("a non-notify request produces a response");
    let message = Message::from_slice(&response).unwrap();
    assert_eq!(message.error_code(), Some(ErrorCode::Ok));
    assert_eq!(message.decode_body::<Vec<i64>>().unwrap(), vec![1, 2, 3]);
}
