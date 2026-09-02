//! Structs that own something, rather than merely holding data.
//!
//! Two dispatch rules used to assume the second shape. Both are about the same
//! sentence — the derive treated "write the whole object" as "replace it" — and
//! both are right for a data struct and wrong for one backed by a resource.
//!
//! * A **whole-child write** assigned over the field instead of descending, so a
//!   `#[repe(nested)]` child's own `RepeStruct` impl was consulted on every path
//!   but that one. It is exactly the path where a child that owns live state has
//!   something to say: applying `{"retries": 5}` to a live settings object is
//!   not the same operation as replacing a struct.
//! * The **whole-object write** at the root was emitted unconditionally, so every
//!   derived struct had to be `DeserializeOwned` whether or not JSON could
//!   describe one. A struct holding an open socket has no such JSON, and the
//!   workaround was a hand-written `Deserialize` that always errors — a runtime
//!   refusal in place of a compile-time one, restated on every such type.
//!
//! `#[repe(no_replace)]` closes the second, by emitting *only* the refusal so
//! the assignment that forced the bound is never generated. It is spelled apart
//! from `#[repe(readonly)]` deliberately: that one, on a field, refuses every
//! write *through* the field, while this refuses one operation on the whole
//! object and leaves every field writable.
//!
//! A third shape sits alongside them: a child that may not be there at all.
//! `RepeStruct for Option<T>` is carried by the crate because a host cannot write
//! it — `Option` is foreign and the trait is not theirs.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use repe::constants::{BodyFormat, ErrorCode, QueryFormat};
use repe::structs::{RequestBody, ResponseBody, StructError, StructResult, path_from_segments};
use repe::{Message, RepeStruct, Router};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A child that owns live state. A whole-object write **applies** the fields it
/// was handed rather than replacing the child, which is what a settings surface
/// means by a write. Unreachable before a whole-child write descended.
#[derive(Default)]
struct Settings {
    retries: u32,
    timeout: u32,
    /// How many writes have been applied, so an *apply* is
    /// distinguishable from a *replace* by more than its result.
    writes: u32,
}
structio::object!(Settings {
    retries,
    timeout,
    writes
});

/// What a whole-object write means for `Settings`: only the keys the caller
/// named move, and each one counts.
///
/// Declared separately from [`Settings`] itself because it is a different
/// shape: every field is optional, so `{"retries": 5}` is a legal document and
/// says nothing about `timeout`. Under a document model this was a map walked
/// key by key; here it is a type, and the absent members simply stay `None`.
#[derive(Default)]
struct SettingsPatch {
    retries: Option<u32>,
    timeout: Option<u32>,
}
structio::object!(SettingsPatch { retries, timeout });

impl RepeStruct for Settings {
    fn repe_handle_into(
        &mut self,
        segments: &[&str],
        body: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> StructResult<()> {
        let field = |this: &Settings, name: &str| match name {
            "retries" => Some(this.retries),
            "timeout" => Some(this.timeout),
            "writes" => Some(this.writes),
            _ => None,
        };
        match (segments, body) {
            ([], None) => {
                out.write(self);
                Ok(())
            }
            // The applying semantics: only the named keys move.
            //
            // Read under `Standard` rather than the wire policy, so a key this
            // surface does not recognize is refused rather than skipped. That
            // is a deliberate narrowing of what the protocol permits, and it is
            // the right one here: a settings write naming a member that does
            // not exist is a caller mistake, and silently applying nothing is
            // the worst available answer.
            ([], Some(body)) => {
                let mut patch = SettingsPatch::default();
                body.read_into_with::<structio::Standard, _>("", &mut patch)?;
                for (name, value) in [("retries", patch.retries), ("timeout", patch.timeout)] {
                    let Some(value) = value else { continue };
                    match name {
                        "retries" => self.retries = value,
                        _ => self.timeout = value,
                    }
                    self.writes += 1;
                }
                out.write_null();
                Ok(())
            }
            ([name], None) => match field(self, name) {
                Some(value) => {
                    out.write(&value);
                    Ok(())
                }
                None => Err(StructError::InvalidPath {
                    path: path_from_segments(segments),
                }),
            },
            _ => Err(StructError::InvalidPath {
                path: path_from_segments(segments),
            }),
        }
    }
}

/// A derived child, for the other half of the guarantee: a whole-child write
/// against one of these still replaces it, because that is what its own
/// empty-segments arm does. Descending changed which impl decides, not what a
/// derived one decides.
#[derive(Default, RepeStruct)]
struct Counter {
    ticks: u64,
    source: String,
}
structio::object!(Counter { ticks, source });

/// The root. No `Deserialize` impl anywhere on it — that this file compiles is
/// half of what `#[repe(no_replace)]` is for.
#[derive(Default, RepeStruct)]
#[repe(no_replace)]
struct Service {
    version: u32,
    #[repe(nested)]
    settings: Settings,
    #[repe(nested)]
    counter: Counter,
    /// A child that is not on every build.
    #[repe(nested)]
    aux: Option<Counter>,
    /// `readonly` on a nested field refuses every write *through* it, sub-paths
    /// included. It previously guarded only the whole-child write, which became
    /// an odd asymmetry once whole-child writes descended.
    #[repe(nested)]
    #[repe(readonly)]
    fixed: Counter,
}
structio::object!(Service {
    version,
    settings,
    counter,
    aux,
    fixed
});

fn service() -> Service {
    Service {
        version: 7,
        settings: Settings::default(),
        counter: Counter {
            ticks: 990,
            source: String::from("local"),
        },
        aux: None,
        fixed: Counter {
            ticks: 1,
            source: String::from("static"),
        },
    }
}

// ---------------------------------------------------------------------------
// Frame helpers
// ---------------------------------------------------------------------------

fn read(query: &str) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .build()
        .to_vec()
}

/// A write whose body is the given JSON text, sent verbatim.
///
/// These tests are about what a *body* means to a struct that owns something,
/// so the body is the text a client would send rather than a declared type.
fn write(query: &str, body: &str) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(body.as_bytes().to_vec())
        .body_format(BodyFormat::Json)
        .build()
        .to_vec()
}

fn answer(router: &Router, request: &[u8]) -> Message {
    let frame = router
        .call(request)
        .expect("a non-notify request is answered");
    Message::from_slice(&frame).expect("the response is a REPE frame")
}

/// The response body as JSON text. Compared as text throughout: with no
/// document model there is nothing between the bytes and the assertion.
fn body(router: &Router, request: &[u8]) -> String {
    let message = answer(router, request);
    std::str::from_utf8(&message.body)
        .expect("the response body is UTF-8 JSON")
        .to_string()
}

/// One router per lock kind: a `Mutex` always takes the exclusive path, an
/// `RwLock` tries the shared one first. Every assertion below is made against
/// both, because which path served a request must not be visible in the answer.
fn routers(build: fn() -> Service) -> [(&'static str, Router); 2] {
    [
        (
            "Mutex",
            Router::new()
                .with_struct_shared::<Service, _>("/service", Arc::new(Mutex::new(build()))),
        ),
        (
            "RwLock",
            Router::new()
                .with_struct_shared::<Service, _>("/service", Arc::new(RwLock::new(build()))),
        ),
    ]
}

// ---------------------------------------------------------------------------
// A whole-child write descends
// ---------------------------------------------------------------------------

#[test]
fn a_whole_child_write_reaches_the_child_s_own_impl() {
    for (kind, router) in routers(service) {
        answer(
            &router,
            &write("/service/settings", r##"{ "retries": 5 }"##),
        );
        assert_eq!(
            body(&router, &read("/service/settings")),
            r##"{"retries":5,"timeout":0,"writes":1}"##,
            "under a {kind} the child applied one key rather than being replaced"
        );

        // Applying again touches only the named key: a replace would have
        // reset `retries` to the default.
        answer(
            &router,
            &write("/service/settings", r##"{ "timeout": 3 }"##),
        );
        assert_eq!(
            body(&router, &read("/service/settings")),
            r##"{"retries":5,"timeout":3,"writes":2}"##,
            "under a {kind} the second apply left the first key alone"
        );
    }
}

#[test]
fn a_whole_child_write_reports_the_child_s_own_error() {
    // The child decides, and its refusal is what reaches the client rather than
    // a generic parent-level one.
    //
    // *Which* refusal changed with the codec. The child used to walk the body
    // key by key and could name the offending one (`/settings/nope`); it now
    // reads into a declared `SettingsPatch` under `structio::Standard`, and the
    // reader refuses the unknown key before the handler sees it. So the error
    // is `InvalidBody` rather than `MethodNotFound`, and it names the byte the
    // parse stopped at rather than the key — a worse message, bought with a
    // decode that does not walk a map.
    //
    // Note the `Standard`: under repe's default wire policy the key would be
    // *skipped*, because that is what REPE's schema-evolution guarantee asks
    // for. This surface opts out, and that opt-out is what this test observes.
    // What the test is about is unchanged: the message is the child's, carried
    // up under the field that owns it.
    for (kind, router) in routers(service) {
        let message = answer(&router, &write("/service/settings", r##"{ "nope": 1 }"##));
        assert_eq!(
            message.error_code(),
            Some(ErrorCode::InvalidBody),
            "under a {kind} the child's refusal is what reaches the client"
        );
        let detail = message
            .error_message_utf8()
            .expect("an error frame carries a message");
        assert!(
            detail.contains("/settings"),
            "under a {kind} the child's error is prefixed with the field: {detail}"
        );
    }
}

#[test]
fn a_derived_child_is_still_replaced_by_a_whole_child_write() {
    // The backward-compatibility half. A derived child's empty-segments arm is
    // `*self = from_value(..)`, so routing the write to the child changed which
    // impl decides and not what a derived one decides.
    for (kind, router) in routers(service) {
        answer(
            &router,
            &write("/service/counter", r##"{ "ticks": 5, "source": "beta" }"##),
        );
        assert_eq!(
            body(&router, &read("/service/counter")),
            r##"{"ticks":5,"source":"beta"}"##,
            "under a {kind} a derived child still replaces"
        );
    }
}

#[test]
fn a_per_field_write_below_a_child_is_unchanged() {
    for (kind, router) in routers(service) {
        answer(&router, &write("/service/counter/ticks", "42"));
        assert_eq!(
            body(&router, &read("/service/counter/ticks")),
            "42",
            "under a {kind} a write below the child still lands on the field"
        );
    }
}

// ---------------------------------------------------------------------------
// A read-only struct
// ---------------------------------------------------------------------------

#[test]
fn a_whole_object_write_against_a_no_replace_struct_is_refused() {
    for (kind, router) in routers(service) {
        let message = answer(&router, &write("/service", r##"{ "version": 9 }"##));
        assert_eq!(
            message.error_code(),
            Some(ErrorCode::InvalidBody),
            "under a {kind} the whole-object write is refused rather than deserialized"
        );
    }
}

#[test]
fn every_write_answers_the_same_under_both_locks() {
    // The refusals above are served by the *shared* borrow — refusing a write
    // touches nothing — while the writes that land take the exclusive one. Which
    // path produced a frame must not be visible in it.
    let [(_, exclusive), (_, shared)] = routers(service);
    for (name, request) in [
        (
            "no_replace root",
            write("/service", r##"{ "version": 9 }"##),
        ),
        ("field write", write("/service/version", "9")),
        (
            "whole child apply",
            write("/service/settings", r##"{ "retries": 5 }"##),
        ),
        (
            "whole child refusal",
            write("/service/settings", r##"{ "nope": 1 }"##),
        ),
        (
            "derived child replace",
            write("/service/counter", r##"{ "ticks": 5, "source": "beta" }"##),
        ),
        ("nested field write", write("/service/counter/ticks", "42")),
        (
            "absent child write",
            write("/service/aux", r##"{ "ticks": 1, "source": "x" }"##),
        ),
    ] {
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "`{name}` must not depend on which guard served it"
        );
    }
}

#[test]
fn a_readonly_refusal_is_served_without_the_exclusive_guard() {
    // Refusing a write touches nothing, so it does not need the write guard —
    // and taking it would put a refusal behind whatever call happens to hold the
    // object. Comparing frames cannot see this; holding a read guard can.
    let state = Arc::new(RwLock::new(service()));
    let router = Router::new().with_struct_shared::<Service, _>("/service", Arc::clone(&state));
    let held = state.read().expect("the lock is not poisoned");

    for (name, request) in [
        (
            "no_replace root",
            write("/service", r##"{ "version": 9 }"##),
        ),
        (
            "readonly child, whole",
            write("/service/fixed", r##"{ "ticks": 9, "source": "x" }"##),
        ),
        (
            "readonly child, sub-path",
            write("/service/fixed/ticks", "9"),
        ),
    ] {
        let router = router.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(router.call(&request));
        });
        let frame = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("refusing `{name}` must not wait for the exclusive guard"))
            .expect("a non-notify request is answered");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        assert_eq!(message.error_code(), Some(ErrorCode::InvalidBody));
    }

    drop(held);
}

#[test]
fn readonly_on_a_nested_field_refuses_every_write_through_it() {
    for (kind, router) in routers(service) {
        for (name, request) in [
            (
                "whole child",
                write("/service/fixed", r##"{ "ticks": 9, "source": "x" }"##),
            ),
            ("sub-path", write("/service/fixed/ticks", "9")),
        ] {
            assert_eq!(
                answer(&router, &request).error_code(),
                Some(ErrorCode::InvalidBody),
                "under a {kind} a {name} write through a read-only child is refused"
            );
        }
        // Reads are untouched, at the child and below it.
        assert_eq!(
            body(&router, &read("/service/fixed")),
            r##"{"ticks":1,"source":"static"}"##,
            "under a {kind} the child still reads whole"
        );
        assert_eq!(body(&router, &read("/service/fixed/ticks")), "1");
    }
}

#[test]
fn a_no_replace_struct_still_reads_and_still_writes_its_parts() {
    for (kind, router) in routers(service) {
        assert_eq!(
            body(&router, &read("/service")),
            concat!(
                r#"{"version":7,"settings":{"retries":0,"timeout":0,"writes":0},"#,
                r#""counter":{"ticks":990,"source":"local"},"aux":null,"#,
                r#""fixed":{"ticks":1,"source":"static"}}"#
            ),
            "under a {kind} the listing is unaffected by `#[repe(no_replace)]`"
        );
        answer(&router, &write("/service/version", "9"));
        assert_eq!(
            body(&router, &read("/service/version")),
            "9",
            "under a {kind} `#[repe(no_replace)]` refuses one operation on the whole object, \
             and leaves its fields writable"
        );
    }
}

// ---------------------------------------------------------------------------
// A child that may not be there
// ---------------------------------------------------------------------------

fn with_aux() -> Service {
    Service {
        aux: Some(Counter {
            ticks: 12,
            source: String::from("alpha"),
        }),
        ..service()
    }
}

#[test]
fn an_absent_child_reads_as_null_and_refuses_everything_else() {
    for (kind, router) in routers(service) {
        assert_eq!(
            body(&router, &read("/service/aux")),
            "null",
            "under a {kind} an absent child reads as null at its own path"
        );
        assert_eq!(
            answer(&router, &read("/service/aux/ticks")).error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} a subpath of an absent child does not resolve"
        );
        assert_eq!(
            answer(
                &router,
                &write("/service/aux", r##"{ "ticks": 1, "source": "x" }"##)
            )
            .error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} configuring an absent child is an error, not a silent no-op"
        );
        assert_eq!(
            answer(&router, &write("/service/aux/ticks", "1")).error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} a subpath write against an absent child is an error too"
        );
    }
}

#[test]
fn a_present_child_forwards_everything_to_the_inner_value() {
    for (kind, router) in routers(with_aux) {
        assert_eq!(
            body(&router, &read("/service/aux")),
            r##"{"ticks":12,"source":"alpha"}"##,
            "under a {kind} a present child reads as itself"
        );
        assert_eq!(body(&router, &read("/service/aux/ticks")), "12");
        answer(&router, &write("/service/aux/ticks", "34"));
        assert_eq!(
            body(&router, &read("/service/aux/ticks")),
            "34",
            "under a {kind} a write below a present child lands on the inner field"
        );
        answer(
            &router,
            &write("/service/aux", r##"{ "ticks": 1, "source": "beta" }"##),
        );
        assert_eq!(
            body(&router, &read("/service/aux")),
            r##"{"ticks":1,"source":"beta"}"##,
            "under a {kind} a whole-child write reaches the inner value's own impl"
        );
    }
}

#[test]
fn an_absent_child_appears_in_its_parent_s_listing() {
    for (kind, router) in routers(service) {
        assert!(
            body(&router, &read("/service")).contains(r#""aux":null"#),
            "under a {kind} the key is present with nothing behind it, rather than the listing \
             failing"
        );
    }
    for (kind, router) in routers(with_aux) {
        assert!(
            body(&router, &read("/service")).contains(r#""aux":{"ticks":12,"source":"alpha"}"#),
            "under a {kind} a present child is listed as itself"
        );
    }
}

#[test]
fn an_option_child_answers_the_same_under_both_locks() {
    // The systematic version of the assertions above: whichever guard served a
    // request, the frame is identical.
    let [(_, exclusive), (_, shared)] = routers(with_aux);
    for path in [
        "/service",
        "/service/aux",
        "/service/aux/ticks",
        "/service/aux/missing",
    ] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` must not depend on which guard served it"
        );
    }
    let [(_, exclusive), (_, shared)] = routers(service);
    for path in ["/service", "/service/aux", "/service/aux/ticks"] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading absent `{path}` must not depend on which guard served it"
        );
    }
}

#[test]
fn presence_is_not_settable_through_the_child_s_own_path() {
    // `null` is what an absent child *reads*, so a client will try writing it
    // back. It is refused either way: honouring it would let a client remove a
    // child and never put it back, since creating one is the write an absent
    // child already refuses.
    for (kind, router) in routers(with_aux) {
        assert_eq!(
            body(&router, &read("/service/aux")),
            r##"{"ticks":12,"source":"alpha"}"##,
            "under a {kind} the child is present to begin with"
        );
        assert_eq!(
            answer(&router, &write("/service/aux", "null")).error_code(),
            Some(ErrorCode::InvalidBody),
            "under a {kind} a `null` write does not clear a present child"
        );
        assert_eq!(
            body(&router, &read("/service/aux")),
            r##"{"ticks":12,"source":"alpha"}"##,
            "under a {kind} it is still there"
        );
    }

    for (kind, router) in routers(service) {
        assert_eq!(
            body(&router, &read("/service/aux")),
            "null",
            "under a {kind} an absent child reads as null"
        );
        assert_eq!(
            answer(&router, &write("/service/aux", "null")).error_code(),
            Some(ErrorCode::InvalidBody),
            "under a {kind} writing that value back is refused rather than a silent no-op"
        );
    }

    // The refusal is served without the exclusive guard, like every other one.
    let state = Arc::new(RwLock::new(with_aux()));
    let router = Router::new().with_struct_shared::<Service, _>("/service", Arc::clone(&state));
    let held = state.read().expect("the lock is not poisoned");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(router.call(&write("/service/aux", "null")));
    });
    let frame = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("refusing a presence write must not wait for the exclusive guard")
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(message.error_code(), Some(ErrorCode::InvalidBody));
    drop(held);

    // And the whole-object write at the parent is a different operation: it
    // replaces the field, `null` included. `Service` is `#[repe(no_replace)]`, so
    // that path is refused here for its own reason; a nested child under a
    // writable parent still replaces.
    assert_eq!(
        answer(
            &Router::new()
                .with_struct_shared::<Service, _>("/service", Arc::new(RwLock::new(with_aux()))),
            &write("/service", r##"{ "aux": null }"##),
        )
        .error_code(),
        Some(ErrorCode::InvalidBody),
    );
}
