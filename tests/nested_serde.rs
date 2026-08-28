//! `#[repe(nested_serde)]`: descend into a field whose type implements only
//! `Serialize` + `DeserializeOwned`.
//!
//! `#[repe(nested)]` emits `<Ty as RepeStruct>::repe_handle(..)`, so nesting an
//! ordinary data struct means deriving `RepeStruct` on it — which means the crate
//! that *declares* it takes a dependency on this one. That choice is not always
//! available: a crate whose stated charter is to stay dependency-light should not
//! have to acquire an RPC layer to recover a couple of sub-paths. The alternative
//! was conceding those paths entirely.
//!
//! Two things close that, and they are not exclusive. The general one is
//! `repe-core`, which carries the trait so a pure crate can implement it without
//! the server, the client, or the transport. This attribute is the other: it
//! works on a type there is no way to annotate at all — one from a crate you do
//! not own.
//!
//! The cost is explicit and paid only on a sub-path: a read materializes the
//! field as a `serde_json::Value` and walks it, and a write materializes it,
//! edits it, and deserializes it back. A whole-field read or write pays neither,
//! and neither does a listing.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use std::{sync::mpsc, thread};

use repe::constants::{ErrorCode, QueryFormat};
use repe::{Message, RepeStruct, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A plain data type. It implements the two serde traits and nothing else —
/// stand-in for a type declared in a crate that cannot depend on this one.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Details {
    size: u64,
    title: String,
    scores: Vec<u32>,
}

/// The same, behind a field that refuses writes.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Origin {
    source: String,
    steps: Vec<u32>,
}

/// A type that does **not** round-trip through serde, which is the attribute's
/// sharpest edge: a sub-path write rebuilds the whole field from its serialized
/// form, so anything serde drops on the way out comes back as `Default`.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Lossy {
    keep: u32,
    /// Never serialized, so a sub-path write to `keep` resets it.
    #[serde(skip)]
    handle: u64,
    /// Absent from the serialized form while it is `None`, so it has no
    /// sub-path until something sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize, RepeStruct)]
struct Record {
    id: String,
    #[repe(nested_serde)]
    details: Details,
    #[repe(nested_serde)]
    #[repe(readonly)]
    origin: Origin,
    #[repe(nested_serde)]
    lossy: Lossy,
}

fn record() -> Record {
    Record {
        id: String::from("r-1"),
        details: Details {
            size: 4096,
            title: String::from("notes"),
            scores: vec![1, 2, 4],
        },
        origin: Origin {
            source: String::from("import"),
            steps: vec![100, 200],
        },
        lossy: Lossy {
            keep: 1,
            handle: 0xdead_beef,
            note: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
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

fn routers() -> [(&'static str, Router); 2] {
    [
        (
            "Mutex",
            Router::new()
                .with_struct_shared::<Record, _>("/record", Arc::new(Mutex::new(record()))),
        ),
        (
            "RwLock",
            Router::new()
                .with_struct_shared::<Record, _>("/record", Arc::new(RwLock::new(record()))),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[test]
fn a_subpath_below_a_serde_field_resolves() {
    for (kind, router) in routers() {
        assert_eq!(
            body(&router, &read("/record/details/size")),
            json!(4096u64),
            "under a {kind} the sub-path a `#[repe(nested)]` field would serve is served here too"
        );
        assert_eq!(
            body(&router, &read("/record/details/title")),
            json!("notes")
        );
        assert_eq!(
            body(&router, &read("/record/details/scores/2")),
            json!(4),
            "under a {kind} an array index is a reference token like any other"
        );
        assert_eq!(
            body(&router, &read("/record/origin/steps")),
            json!([100, 200]),
            "under a {kind} a read-only field still reads"
        );
    }
}

#[test]
fn the_whole_field_reads_as_itself() {
    for (kind, router) in routers() {
        assert_eq!(
            body(&router, &read("/record/details")),
            json!({
                "size": 4096u64,
                "title": "notes",
                "scores": [1, 2, 4],
            }),
            "under a {kind} the whole field is what `Serialize` produces"
        );
    }
}

#[test]
fn a_serde_field_is_listed_by_its_value() {
    for (kind, router) in routers() {
        assert_eq!(
            body(&router, &read("/record")),
            json!({
                "id": "r-1",
                "details": {
                    "size": 4096u64,
                    "title": "notes",
                    "scores": [1, 2, 4],
                },
                "origin": { "source": "import", "steps": [100, 200] },
                "lossy": { "keep": 1 },
            }),
            "under a {kind} the listing is what it was before the attribute: the descent is a \
             sub-path concern"
        );
    }
}

#[test]
fn a_subpath_that_does_not_resolve_is_method_not_found() {
    for (kind, router) in routers() {
        for path in [
            "/record/details/missing",
            "/record/details/scores/9",
            "/record/details/title/deeper",
        ] {
            assert_eq!(
                answer(&router, &read(path)).error_code(),
                Some(ErrorCode::MethodNotFound),
                "under a {kind} `{path}` does not resolve"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

#[test]
fn a_subpath_write_lands_on_the_field() {
    for (kind, router) in routers() {
        answer(&router, &write("/record/details/size", &2048u64));
        assert_eq!(
            body(&router, &read("/record/details")),
            json!({
                "size": 2048u64,
                "title": "notes",
                "scores": [1, 2, 4],
            }),
            "under a {kind} the write edits one key and leaves the rest alone"
        );

        answer(&router, &write("/record/details/scores/0", &7u32));
        assert_eq!(
            body(&router, &read("/record/details/scores")),
            json!([7, 2, 4]),
            "under a {kind} an array element is writable by index"
        );
    }
}

#[test]
fn a_whole_field_write_replaces_it() {
    for (kind, router) in routers() {
        answer(
            &router,
            &write(
                "/record/details",
                &json!({ "size": 1, "title": "x", "scores": [] }),
            ),
        );
        assert_eq!(
            body(&router, &read("/record/details")),
            json!({ "size": 1, "title": "x", "scores": [] }),
            "under a {kind} the whole field is replaced, as a plain field is"
        );
    }
}

#[test]
fn a_write_to_a_key_the_field_does_not_have_is_refused() {
    for (kind, router) in routers() {
        assert_eq!(
            answer(&router, &write("/record/details/missing", &1u32)).error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} a misspelled key is an error rather than a silently added one"
        );
        // And nothing moved.
        assert_eq!(
            body(&router, &read("/record/details/title")),
            json!("notes"),
            "under a {kind} the refused write left the field alone"
        );
    }
}

#[test]
fn a_write_of_the_wrong_type_is_refused_and_leaves_the_field_intact() {
    for (kind, router) in routers() {
        assert_eq!(
            answer(&router, &write("/record/details/size", &"not a number")).error_code(),
            Some(ErrorCode::InvalidBody),
            "under a {kind} the edited value has to deserialize back into the field's type"
        );
        assert_eq!(
            body(&router, &read("/record/details/size")),
            json!(4096u64),
            "under a {kind} the field is only assigned once the whole value decodes"
        );
    }
}

#[test]
fn readonly_refuses_every_write_through_the_field() {
    for (kind, router) in routers() {
        for request in [
            write("/record/origin", &json!({ "source": "x", "steps": [] })),
            write("/record/origin/source", &"x"),
            write("/record/origin/steps/0", &1u32),
        ] {
            assert_eq!(
                answer(&router, &request).error_code(),
                Some(ErrorCode::InvalidBody),
                "under a {kind} `#[repe(readonly)]` covers the sub-paths as well as the field"
            );
        }
        assert_eq!(
            body(&router, &read("/record/origin")),
            json!({ "source": "import", "steps": [100, 200] })
        );
    }
}

// ---------------------------------------------------------------------------
// The two guards agree, and reads are served shared
// ---------------------------------------------------------------------------

#[test]
fn every_path_answers_the_same_under_a_mutex_and_an_rwlock() {
    let [(_, exclusive), (_, shared)] = routers();
    for path in [
        "/record",
        "/record/details",
        "/record/details/title",
        "/record/details/scores/1",
        "/record/details/missing",
        "/record/origin/source",
    ] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` must not depend on which guard served it"
        );
    }

    for (name, request) in [
        ("subpath write", write("/record/details/title", &"revised")),
        (
            "whole field write",
            write(
                "/record/details",
                &json!({ "size": 3, "title": "z", "scores": [1] }),
            ),
        ),
        ("readonly refusal", write("/record/origin/source", &"x")),
        ("unresolved write", write("/record/details/nope", &1u32)),
    ] {
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "`{name}` must not depend on which guard served it"
        );
    }
}

#[test]
fn a_subpath_read_proceeds_while_a_read_guard_is_held() {
    // Walking a `serde_json::Value` of the field mutates nothing, so the whole
    // descent is servable under a shared borrow.
    let state = Arc::new(RwLock::new(record()));
    let router = Router::new().with_struct_shared::<Record, _>("/record", Arc::clone(&state));
    let held = state.read().expect("the lock is not poisoned");

    for path in [
        "/record/details",
        "/record/details/size",
        "/record/details/scores/1",
        "/record/origin/steps",
    ] {
        let router = router.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(router.call(&read(path)));
        });
        let frame = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("reading `{path}` must not wait for the exclusive guard"))
            .expect("a non-notify request is answered");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        assert!(
            !message.is_error(),
            "reading `{path}` answered with an error"
        );
    }

    drop(held);
}

// ---------------------------------------------------------------------------
// The round trip, and what it costs
// ---------------------------------------------------------------------------

#[test]
fn a_subpath_write_resets_whatever_serde_drops() {
    // The attribute's sharpest edge, pinned so it cannot change silently. A
    // sub-path write rebuilds the *whole* field from its serialized form, so a
    // `#[serde(skip)]` field is reset by a write to an unrelated sibling key.
    // `docs/server.md` says so, and says to reach for `#[repe(nested)]` or
    // `#[repe(readonly)]` when the type does not round-trip; this is why.
    //
    // Asserted against the live value rather than a read, because a field serde
    // drops has no endpoint to read it back through — which is exactly what
    // makes the loss quiet.
    let state = Arc::new(RwLock::new(record()));
    let router = Router::new().with_struct_shared::<Record, _>("/record", Arc::clone(&state));
    assert_eq!(state.read().unwrap().lossy.handle, 0xdead_beef);

    answer(&router, &write("/record/lossy/keep", &7u32));

    let held = state.read().unwrap();
    assert_eq!(held.lossy.keep, 7, "the write landed");
    assert_eq!(
        held.lossy.handle, 0,
        "and the `#[serde(skip)]` sibling went back to `Default` on the way through"
    );
}

#[test]
fn a_subpath_absent_from_the_serialized_form_does_not_resolve() {
    for (kind, router) in routers() {
        // `note` is `None` and `skip_serializing_if` drops it, so it is not a
        // key of the value the walk sees.
        assert_eq!(
            answer(&router, &read("/record/lossy/note")).error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} a conditionally-skipped field has no sub-path while it is skipped"
        );
        assert_eq!(
            answer(&router, &write("/record/lossy/note", &"hi")).error_code(),
            Some(ErrorCode::MethodNotFound),
            "under a {kind} it cannot be written into existence either"
        );

        // Set through the whole field, and the sub-path appears.
        answer(
            &router,
            &write("/record/lossy", &json!({ "keep": 2, "note": "hi" })),
        );
        assert_eq!(body(&router, &read("/record/lossy/note")), json!("hi"));
    }
}
