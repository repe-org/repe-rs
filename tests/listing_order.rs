//! `#[repe(listing_order(..))]`: the key order of the whole-object listing.
//!
//! Without it the listing appends in a fixed order — fields in declaration
//! order, then struct-listed methods, then the `#[repe::methods]` block's
//! signatures, then its field-shaped accessors. Both sinks, no sort, no hook. So
//! a `#[repe(get/set)]` endpoint is always *last*, wherever its logical place
//! reads.
//!
//! That is wire-visible, and it is the one key order that cannot be reproduced
//! from the other side of an interop pair: `glz::object` lists in the order
//! written, so a C++ object with a `custom<setter, getter>` in the middle has a
//! shape this derive could not emit.
//!
//! The attribute names the whole sequence, and naming it in full is what makes a
//! typo or an omission a compile error rather than a silently missing key. Half
//! of that check is at macro time, against the fields and the struct-level
//! method list; the other half is a `const` assertion against the impl block's
//! two generated tables, which the derive cannot see.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::{Arc, Mutex, RwLock};

use repe::constants::QueryFormat;
use repe::{Message, RepeStruct, Router};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The shape that motivates the attribute: a field-shaped endpoint whose
/// logical place is in the middle of the fields, not after them.
#[derive(Clone, Default, RepeStruct)]
#[repe(methods)]
#[repe(listing_order("name", "count", "percent", "total", "identify", "reset"))]
struct Ordered {
    name: String,
    count: u32,
    total: f64,
    #[repe(skip)]
    ratio: f64,
}
structio::object!(Ordered {
    name,
    count,
    total,
    ratio
});

#[repe::methods]
impl Ordered {
    #[repe(get = "percent")]
    fn percent(&self) -> f64 {
        self.ratio * 100.0
    }

    #[repe(set = "percent")]
    fn set_percent(&mut self, pct: f64) {
        self.ratio = pct / 100.0;
    }

    fn identify(&self) -> String {
        format!("item {name}", name = self.name)
    }

    fn reset(&mut self) {}
}

/// The same surface with no `listing_order`, so the default is pinned beside the
/// override rather than only implied by it.
#[derive(Clone, Default, RepeStruct)]
#[repe(methods)]
struct Appended {
    name: String,
    count: u32,
    total: f64,
    #[repe(skip)]
    ratio: f64,
}
structio::object!(Appended {
    name,
    count,
    total,
    ratio
});

#[repe::methods]
impl Appended {
    #[repe(get = "percent")]
    fn percent(&self) -> f64 {
        self.ratio * 100.0
    }

    fn identify(&self) -> String {
        format!("item {name}", name = self.name)
    }

    fn reset(&mut self) {}
}

/// A `#[repe::methods]` block that compiles and publishes **nothing**.
///
/// Both generated tables are empty, which is also the shape the attribute emits
/// when the block failed to parse. The const assertion stands down for that
/// recovery shape so a real error is not buried under a second one, and it has
/// to tell the two apart by the marker rather than by emptiness: the derive
/// skips its own unknown-key check whenever an impl block is in play, so if the
/// assertion also stood down here, `#[repe(listing_order("a", "typo"))]` would
/// compile and then fail *every* whole-object read with `InvalidPath` while each
/// endpoint still answered on its own path.
#[derive(Clone, Default, RepeStruct)]
#[repe(methods)]
#[repe(listing_order("beta", "alpha"))]
struct Quiet {
    alpha: u32,
    beta: u32,
}
structio::object!(Quiet { alpha, beta });

#[repe::methods]
impl Quiet {
    #[repe(skip)]
    fn hidden(&self) -> u32 {
        self.alpha
    }
}

/// A struct with no `#[repe::methods]` block: everything the order names is
/// visible to the derive, so the whole check happens at macro time.
#[derive(Clone, Default, RepeStruct)]
#[repe(methods(probe(&self) -> u32))]
#[repe(listing_order("beta", "probe", "alpha"))]
struct Reversed {
    alpha: u32,
    beta: u32,
}
structio::object!(Reversed { alpha, beta });

impl Reversed {
    fn probe(&self) -> u32 {
        self.alpha + self.beta
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

/// The top-level keys of a JSON object, in the order the document carries them.
///
/// A declared type cannot answer this: it names the keys it expects, and the
/// question here is what order the *bytes* put them in. So this reads the text
/// rather than a value — one scan for the key strings at depth 1, which is all
/// the assertion needs and exactly what a client would see.
fn key_order(json: &str) -> Vec<String> {
    let bytes = json.as_bytes();
    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    let mut expecting_key = false;

    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                expecting_key = depth == 1;
            }
            b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                expecting_key = false;
            }
            b',' => expecting_key = depth == 1,
            b'"' => {
                // Read the string, honoring escapes so a `\"` inside it does
                // not end it early.
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += if bytes[j] == b'\\' { 2 } else { 1 };
                }
                if expecting_key {
                    keys.push(json[start..j].to_string());
                    expecting_key = false;
                }
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    keys
}

/// `RepeStruct::repe_handle_into`, writes members straight into the response
/// buffer in declaration order, so the order the derive emits *is* the order on
/// the wire. Under `serde_json` this was not assertable at all: the listing was
/// assembled into a `Map` — a `BTreeMap` unless something in the graph enabled
/// `preserve_order` — which sorted its keys and could carry no order.
fn listing_keys(router: &Router, query: &str) -> Vec<String> {
    let frame = router
        .call(&read(query))
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    let text = std::str::from_utf8(&message.body).expect("the listing body is UTF-8");
    key_order(text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn a_listing_order_places_an_accessor_among_the_fields() {
    // Both lock kinds, because the exclusive listing and the shared one are
    // generated separately and a reordering that reached only one of them would
    // be a difference a client could see.
    for (kind, router) in [
        (
            "Mutex",
            Router::new()
                .with_struct_shared::<Ordered, _>("/o", Arc::new(Mutex::new(Ordered::default()))),
        ),
        (
            "RwLock",
            Router::new()
                .with_struct_shared::<Ordered, _>("/o", Arc::new(RwLock::new(Ordered::default()))),
        ),
    ] {
        assert_eq!(
            listing_keys(&router, "/o"),
            ["name", "count", "percent", "total", "identify", "reset"],
            "under a {kind} the listing emits the order the attribute names"
        );
    }
}

#[test]
fn the_default_order_appends_accessors_last() {
    let router = Router::new()
        .with_struct_shared::<Appended, _>("/a", Arc::new(RwLock::new(Appended::default())));
    assert_eq!(
        listing_keys(&router, "/a"),
        ["name", "count", "total", "identify", "reset", "percent"],
        "without the attribute, fields come first and field-shaped endpoints last"
    );
}

#[test]
fn an_order_can_interleave_a_struct_level_method() {
    let router = Router::new()
        .with_struct_shared::<Reversed, _>("/r", Arc::new(RwLock::new(Reversed::default())));
    assert_eq!(listing_keys(&router, "/r"), ["beta", "probe", "alpha"]);
}

#[test]
fn reordering_the_listing_changes_nothing_else() {
    let ordered = Ordered {
        name: String::from("sn-1"),
        count: 4,
        total: 36.5,
        ratio: 0.25,
    };
    let router = Router::new()
        .with_struct_shared::<Ordered, _>("/o", Arc::new(RwLock::new(ordered.clone())));

    let frame = router
        .call(&read("/o"))
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    // Compared as text, and that is the point: the assertion is on the bytes
    // the listing produced, order included. A declared type would have named
    // the keys and told us nothing about their order.
    let text = std::str::from_utf8(&message.body).expect("the listing body is UTF-8");
    assert_eq!(
        text,
        concat!(
            r#"{"name":"sn-1","count":4,"percent":25,"total":36.5,"#,
            r#""identify":"fn(&self) -> String","reset":"fn(&mut self) -> ()"}"#
        ),
        "the attribute reorders emission and nothing else: same keys, same values"
    );

    // Individual endpoints are untouched by it.
    let frame = router
        .call(&read("/o/percent"))
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(message.json_body::<f64>().unwrap(), 25.0);
}

#[test]
fn an_order_is_still_checked_against_a_block_that_publishes_nothing() {
    // The runtime half of the comment on `Quiet`. The compile-time half is that
    // this file compiles at all: naming a key that is not an endpoint here is an
    // `E0080` at the attribute, and it would not be if the assertion mistook an
    // empty table for a failed one.
    let router =
        Router::new().with_struct_shared::<Quiet, _>("/q", Arc::new(RwLock::new(Quiet::default())));
    assert_eq!(
        listing_keys(&router, "/q"),
        ["beta", "alpha"],
        "a block that publishes nothing still gets its order emitted"
    );
    assert_eq!(
        Quiet::default().hidden(),
        0,
        "`#[repe(skip)]` takes the method off the wire, not off the type"
    );
}
