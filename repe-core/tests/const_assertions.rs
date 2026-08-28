//! The compile-time assertions the derive wires into every struct that needs
//! them, exercised as ordinary functions.
//!
//! These are `const fn`, so the derive calls them in a `const` block and a
//! violation is an `E0080` at the offending attribute. That is what a user
//! sees, and it is not what this file checks: a `const fn` is callable at run
//! time too, and calling it here pins the *message* — the sentence the user has
//! to act on — without pinning rustc's rendering of it around the panic, which
//! changes between releases and would make this file a maintenance tax rather
//! than a test.
//!
//! What proves the wiring, as opposed to the message, is that
//! `tests/listing_order.rs` compiles at all: every assertion there runs at
//! compile time on a real derived struct.

use repe_core::structs::{assert_listing_order, assert_no_endpoint_collision};

// ---------------------------------------------------------------------------
// `#[repe(listing_order(..))]`
// ---------------------------------------------------------------------------

#[test]
fn an_order_naming_every_endpoint_passes() {
    assert_listing_order(
        &["a", "calc", "ratio"],
        &["a"],
        &[("calc", "fn(&self) -> u64")],
        &["ratio"],
        false,
    );
}

#[test]
#[should_panic(expected = "names a key that is not an endpoint on this struct")]
fn an_order_naming_an_unknown_key_is_rejected() {
    assert_listing_order(&["a", "typo"], &["a"], &[], &[], false);
}

#[test]
#[should_panic(expected = "is missing from `#[repe(listing_order(..))]`")]
fn an_order_omitting_a_method_is_rejected() {
    assert_listing_order(&["a"], &["a"], &[("calc", "fn(&self) -> u64")], &[], false);
}

#[test]
#[should_panic(expected = "is missing from `#[repe(listing_order(..))]`")]
fn an_order_omitting_an_accessor_is_rejected() {
    assert_listing_order(&["a"], &["a"], &[], &["ratio"], false);
}

#[test]
fn a_recovering_table_stands_every_check_down() {
    // The placeholder `#[repe::methods]` emits when the block itself failed to
    // parse. Its tables publish nothing, so every check against them would fire
    // and bury the real error under a second, misleading one.
    assert_listing_order(&["a", "typo"], &["a"], &[], &[], true);
}

#[test]
#[should_panic(expected = "names a key that is not an endpoint on this struct")]
fn an_empty_table_that_is_not_recovering_is_still_checked() {
    // The hole this pairs with: a block that compiles and publishes nothing
    // produces the same empty tables as a failed one, and telling them apart by
    // emptiness would let a typo through. Only the marker may stand the check
    // down.
    assert_listing_order(&["a", "typo"], &["a"], &[], &[], false);
}

// ---------------------------------------------------------------------------
// Endpoint collisions
// ---------------------------------------------------------------------------

#[test]
fn three_disjoint_endpoint_sets_pass() {
    assert_no_endpoint_collision(&["a"], &[("calc", "fn(&self) -> u64")], &["ratio"]);
}

#[test]
#[should_panic(expected = "the struct's declaration wins dispatch")]
fn a_method_colliding_with_a_struct_declaration_is_rejected() {
    assert_no_endpoint_collision(&["calc"], &[("calc", "fn(&self) -> u64")], &[]);
}

#[test]
#[should_panic(expected = "the struct's declaration wins dispatch")]
fn an_accessor_colliding_with_a_struct_declaration_is_rejected() {
    assert_no_endpoint_collision(&["ratio"], &[], &["ratio"]);
}

#[test]
#[should_panic(expected = "one of them is unreachable")]
fn a_method_colliding_with_an_accessor_is_rejected() {
    assert_no_endpoint_collision(&[], &[("ratio", "fn(&self) -> u64")], &["ratio"]);
}
