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
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The shape that motivates the attribute: a field-shaped endpoint whose
/// logical place is in the middle of the fields, not after them.
#[derive(Clone, Default, Serialize, Deserialize, RepeStruct)]
#[repe(methods)]
#[repe(listing_order("name", "count", "percent", "total", "identify", "reset"))]
struct Ordered {
    name: String,
    count: u32,
    total: f64,
    #[repe(skip)]
    ratio: f64,
}

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
#[derive(Clone, Default, Serialize, Deserialize, RepeStruct)]
#[repe(methods)]
struct Appended {
    name: String,
    count: u32,
    total: f64,
    #[repe(skip)]
    ratio: f64,
}

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

/// A struct with no `#[repe::methods]` block: everything the order names is
/// visible to the derive, so the whole check happens at macro time.
#[derive(Clone, Default, Serialize, Deserialize, RepeStruct)]
#[repe(methods(probe(&self) -> u32))]
#[repe(listing_order("beta", "probe", "alpha"))]
struct Reversed {
    alpha: u32,
    beta: u32,
}

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
/// `serde_json::Value` cannot answer this: its map is sorted unless the
/// `preserve_order` feature is on, and the whole point here is what the *bytes*
/// say.
struct KeyOrder(Vec<String>);

impl<'de> Deserialize<'de> for KeyOrder {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Keys;

        impl<'de> serde::de::Visitor<'de> for Keys {
            type Value = KeyOrder;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<KeyOrder, A::Error> {
                let mut keys = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    map.next_value::<serde::de::IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(KeyOrder(keys))
            }
        }

        deserializer.deserialize_map(Keys)
    }
}

/// Deliberately driven through the `Router`, which encodes through
/// `RepeStruct::repe_handle_into`. The `serde_json::Value` form,
/// `RepeStruct::repe_handle`, assembles a `serde_json::Map` — a `BTreeMap`
/// unless something in the dependency graph enables `serde_json/preserve_order`
/// — so it sorts its keys and can carry no order at all. That is a property of
/// `Value`, true of declaration order since long before this attribute, and not
/// something the derive can fix; asserting it here would pin a `serde_json`
/// build detail rather than anything this crate decides.
fn listing_keys(router: &Router, query: &str) -> Vec<String> {
    let frame = router
        .call(&read(query))
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    message
        .json_body::<KeyOrder>()
        .expect("the listing body is valid JSON")
        .0
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
    let value = message
        .json_body::<serde_json::Value>()
        .expect("the listing body is valid JSON");
    assert_eq!(
        value,
        serde_json::json!({
            "name": "sn-1",
            "count": 4,
            "percent": 25.0,
            "total": 36.5,
            "identify": "fn(&self) -> String",
            "reset": "fn(&mut self) -> ()",
        }),
        "the attribute reorders emission and nothing else: same keys, same values"
    );

    // Individual endpoints are untouched by it.
    let frame = router
        .call(&read("/o/percent"))
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(message.json_body::<f64>().unwrap(), 25.0);
}
