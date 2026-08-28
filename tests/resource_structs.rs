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
//! `#[repe(readonly)]` on the struct closes the second, and closes it the way the
//! field attribute of the same name does: by emitting *only* the refusal, so the
//! assignment that forced the bound is never generated.
//!
//! A third shape sits alongside them: a child that may not be there at all.
//! `RepeStruct for Option<T>` is carried by the crate because a host cannot write
//! it — `Option` is foreign and the trait is not theirs.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use repe::constants::{ErrorCode, QueryFormat};
use repe::structs::{StructError, StructResult, path_from_segments};
use repe::{Message, RepeStruct, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A child that owns live state. A whole-object write **applies** the fields it
/// was handed rather than replacing the child, which is what a settings surface
/// means by a write. Unreachable before a whole-child write descended.
#[derive(Default, Serialize)]
struct Settings {
    retries: u32,
    timeout: u32,
    /// How many writes have been applied, so an *apply* is
    /// distinguishable from a *replace* by more than its result.
    writes: u32,
}

impl RepeStruct for Settings {
    fn repe_handle(
        &mut self,
        segments: &[&str],
        body: Option<Value>,
    ) -> StructResult<Option<Value>> {
        let field = |this: &Settings, name: &str| match name {
            "retries" => Some(this.retries),
            "timeout" => Some(this.timeout),
            "writes" => Some(this.writes),
            _ => None,
        };
        match (segments, body) {
            ([], None) => Ok(Some(json!({
                "retries": self.retries,
                "timeout": self.timeout,
                "writes": self.writes,
            }))),
            // The applying semantics: only the named keys move.
            ([], Some(Value::Object(map))) => {
                for (key, value) in map {
                    let value: u32 = serde_json::from_value(value).map_err(|source| {
                        StructError::Deserialize {
                            path: format!("/{key}"),
                            source,
                        }
                    })?;
                    match key.as_str() {
                        "retries" => self.retries = value,
                        "timeout" => self.timeout = value,
                        _ => {
                            return Err(StructError::InvalidPath {
                                path: format!("/{key}"),
                            });
                        }
                    }
                    self.writes += 1;
                }
                Ok(None)
            }
            ([], Some(_)) => Err(StructError::Deserialize {
                path: String::new(),
                source: serde::de::Error::custom("settings are written as an object"),
            }),
            ([name], None) => field(self, name)
                .map(|value| Some(json!(value)))
                .ok_or_else(|| StructError::InvalidPath {
                    path: path_from_segments(segments),
                }),
            _ => Err(StructError::InvalidPath {
                path: path_from_segments(segments),
            }),
        }
    }
}

/// A derived child, for the other half of the guarantee: a whole-child write
/// atimeoutst one of these still replaces it, because that is what its own
/// empty-segments arm does. Descending changed which impl decides, not what a
/// derived one decides.
#[derive(Default, Serialize, Deserialize, RepeStruct)]
struct Counter {
    ticks: u64,
    source: String,
}

/// The root. No `Deserialize` impl anywhere on it — that this file compiles is
/// half of what `#[repe(readonly)]` is for.
#[derive(Default, Serialize, RepeStruct)]
#[repe(readonly)]
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

fn write<T: Serialize>(query: &str, value: &T) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .body_json(value)
        .expect("the fixtures serialize")
        .build()
        .to_vec()
}

fn answer(router: &Router, request: &[u8]) -> Message {
    let frame = router
        .call(request)
        .expect("a non-notify request is answered");
    Message::from_slice(&frame).expect("the response is a REPE frame")
}

fn body(router: &Router, request: &[u8]) -> Value {
    answer(router, request)
        .json_body::<Value>()
        .expect("the response body is valid JSON")
}

/// One router per lock kind: a `Mutex` always takes the exclusive path, an
/// `RwLock` tries the shared one first. Every assertion below is made atimeoutst
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
            &write("/service/settings", &json!({ "retries": 5 })),
        );
        assert_eq!(
            body(&router, &read("/service/settings")),
            json!({ "retries": 5, "timeout": 0, "writes": 1 }),
            "under a {kind} the child applied one key rather than being replaced"
        );

        // Applying atimeout touches only the named key: a replace would have
        // reset `retries` to the default.
        answer(
            &router,
            &write("/service/settings", &json!({ "timeout": 3 })),
        );
        assert_eq!(
            body(&router, &read("/service/settings")),
            json!({ "retries": 5, "timeout": 3, "writes": 2 }),
            "under a {kind} the second apply left the first key alone"
        );
    }
}

#[test]
fn a_whole_child_write_reports_the_child_s_own_error() {
    for (kind, router) in routers(service) {
        let message = answer(&router, &write("/service/settings", &json!({ "nope": 1 })));
        assert_eq!(
            message.error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} the child's refusal is what reaches the client"
        );
        let detail = message
            .error_message_utf8()
            .expect("an error frame carries a message");
        assert!(
            detail.contains("/settings/nope"),
            "under a {kind} the child's path is prefixed with the field: {detail}"
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
            &write("/service/counter", &json!({ "ticks": 5, "source": "beta" })),
        );
        assert_eq!(
            body(&router, &read("/service/counter")),
            json!({ "ticks": 5, "source": "beta" }),
            "under a {kind} a derived child still replaces"
        );
    }
}

#[test]
fn a_per_field_write_below_a_child_is_unchanged() {
    for (kind, router) in routers(service) {
        answer(&router, &write("/service/counter/ticks", &42u64));
        assert_eq!(
            body(&router, &read("/service/counter/ticks")),
            json!(42),
            "under a {kind} a write below the child still lands on the field"
        );
    }
}

// ---------------------------------------------------------------------------
// A read-only struct
// ---------------------------------------------------------------------------

#[test]
fn a_whole_object_write_atimeoutst_a_readonly_struct_is_refused() {
    for (kind, router) in routers(service) {
        let message = answer(&router, &write("/service", &json!({ "version": 9 })));
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
        ("readonly root", write("/service", &json!({ "version": 9 }))),
        ("field write", write("/service/version", &9u32)),
        (
            "whole child apply",
            write("/service/settings", &json!({ "retries": 5 })),
        ),
        (
            "whole child refusal",
            write("/service/settings", &json!({ "nope": 1 })),
        ),
        (
            "derived child replace",
            write("/service/counter", &json!({ "ticks": 5, "source": "beta" })),
        ),
        (
            "nested field write",
            write("/service/counter/ticks", &42u64),
        ),
        (
            "absent child write",
            write("/service/aux", &json!({ "ticks": 1, "source": "x" })),
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
        ("readonly root", write("/service", &json!({ "version": 9 }))),
        (
            "readonly child, whole",
            write("/service/fixed", &json!({ "ticks": 9, "source": "x" })),
        ),
        (
            "readonly child, sub-path",
            write("/service/fixed/ticks", &9u64),
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
                write("/service/fixed", &json!({ "ticks": 9, "source": "x" })),
            ),
            ("sub-path", write("/service/fixed/ticks", &9u64)),
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
            json!({ "ticks": 1, "source": "static" }),
            "under a {kind} the child still reads whole"
        );
        assert_eq!(body(&router, &read("/service/fixed/ticks")), json!(1));
    }
}

#[test]
fn a_readonly_struct_still_reads_and_still_writes_its_parts() {
    for (kind, router) in routers(service) {
        assert_eq!(
            body(&router, &read("/service")),
            json!({
                "version": 7,
                "settings": { "retries": 0, "timeout": 0, "writes": 0 },
                "counter": { "ticks": 990, "source": "local" },
                "aux": null,
                "fixed": { "ticks": 1, "source": "static" },
            }),
            "under a {kind} the listing is unaffected by `#[repe(readonly)]`"
        );
        answer(&router, &write("/service/version", &9u32));
        assert_eq!(
            body(&router, &read("/service/version")),
            json!(9),
            "under a {kind} `#[repe(readonly)]` on the struct governs the whole object, not \
             its fields"
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
            Value::Null,
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
                &write("/service/aux", &json!({ "ticks": 1, "source": "x" }))
            )
            .error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} configuring an absent child is an error, not a silent no-op"
        );
        assert_eq!(
            answer(&router, &write("/service/aux/ticks", &1u64)).error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} a subpath write atimeoutst an absent child is an error too"
        );
    }
}

#[test]
fn a_present_child_forwards_everything_to_the_inner_value() {
    for (kind, router) in routers(with_aux) {
        assert_eq!(
            body(&router, &read("/service/aux")),
            json!({ "ticks": 12, "source": "alpha" }),
            "under a {kind} a present child reads as itself"
        );
        assert_eq!(body(&router, &read("/service/aux/ticks")), json!(12));
        answer(&router, &write("/service/aux/ticks", &34u64));
        assert_eq!(
            body(&router, &read("/service/aux/ticks")),
            json!(34),
            "under a {kind} a write below a present child lands on the inner field"
        );
        answer(
            &router,
            &write("/service/aux", &json!({ "ticks": 1, "source": "beta" })),
        );
        assert_eq!(
            body(&router, &read("/service/aux")),
            json!({ "ticks": 1, "source": "beta" }),
            "under a {kind} a whole-child write reaches the inner value's own impl"
        );
    }
}

#[test]
fn an_absent_child_appears_in_its_parent_s_listing() {
    for (kind, router) in routers(service) {
        assert_eq!(
            body(&router, &read("/service"))["aux"],
            Value::Null,
            "under a {kind} the key is present with nothing behind it, rather than the listing \
             failing"
        );
    }
    for (kind, router) in routers(with_aux) {
        assert_eq!(
            body(&router, &read("/service"))["aux"],
            json!({ "ticks": 12, "source": "alpha" }),
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
            json!({ "ticks": 12, "source": "alpha" }),
            "under a {kind} the child is present to begin with"
        );
        assert_eq!(
            answer(&router, &write("/service/aux", &Value::Null)).error_code(),
            Some(ErrorCode::InvalidBody),
            "under a {kind} a `null` write does not clear a present child"
        );
        assert_eq!(
            body(&router, &read("/service/aux")),
            json!({ "ticks": 12, "source": "alpha" }),
            "under a {kind} it is still there"
        );
    }

    for (kind, router) in routers(service) {
        assert_eq!(
            body(&router, &read("/service/aux")),
            Value::Null,
            "under a {kind} an absent child reads as null"
        );
        assert_eq!(
            answer(&router, &write("/service/aux", &Value::Null)).error_code(),
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
        let _ = tx.send(router.call(&write("/service/aux", &Value::Null)));
    });
    let frame = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("refusing a presence write must not wait for the exclusive guard")
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(message.error_code(), Some(ErrorCode::InvalidBody));
    drop(held);

    // And the whole-object write at the parent is a different operation: it
    // replaces the field, `null` included. `Service` is `#[repe(readonly)]`, so
    // that path is refused here for its own reason; a nested child under a
    // writable parent still replaces.
    assert_eq!(
        answer(
            &Router::new()
                .with_struct_shared::<Service, _>("/service", Arc::new(RwLock::new(with_aux()))),
            &write("/service", &json!({ "aux": null })),
        )
        .error_code(),
        Some(ErrorCode::InvalidBody),
    );
}
