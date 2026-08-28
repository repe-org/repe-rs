//! The `RepeStruct` surface of [`repe`], on its own.
//!
//! A served struct's endpoints are declared where the *type* is declared, and
//! that is not always the crate that runs the RPC. A pure-logic crate — no I/O,
//! no `unsafe`, buildable on any host, and often kept that way deliberately —
//! should not have to acquire a server, a client, and a transport just to
//! publish a couple of paths. Its choices without this crate were to derive
//! nothing and concede the sub-paths, or to take the whole dependency.
//!
//! So this carries the four things a type needs to *be* served and nothing that
//! serves it:
//!
//! * [`RepeStruct`], the dispatch trait, and [`RepeMethods`], the method table
//!   `#[repe::methods]` generates beside it;
//! * [`StructError`], what a handler reports;
//! * [`ResponseBody`], what a read encodes into;
//! * [`constants`], the protocol enums those two name.
//!
//! `repe` re-exports every one of them at the same paths, so a type derived
//! against this crate mounts on a `repe::Router` with nothing in between, and
//! `#[derive(RepeStruct)]` resolves its generated paths against whichever of the
//! two crates it finds.
//!
//! # Features
//!
//! **`typed`** — off by default — adds [`ResponseBody::write_typed_slice`], the
//! BEVE typed-numeric array encoding a `#[repe(typed)]` field is read as, and
//! with it the `beve` dependency and the six packages behind it.
//!
//! It is off because that encoder is the whole weight of this crate: without it
//! the dependency list is `serde`, `serde_json` and `thiserror`, which is the
//! point of depending here rather than on `repe`. A struct with no
//! `#[repe(typed)]` field — the usual case for a crate that only declares a
//! served type — compiles identically either way. One that has such a field
//! gets a compile error naming the feature, from a bound with no implementors
//! (`structs::TypedSliceElement`, which exists only in this configuration); it
//! never falls back to a JSON array silently, since that would change the wire
//! without saying so.
//!
//! `repe` enables it unconditionally, so nothing about `#[repe(typed)]` changes
//! for a crate that depends on `repe`.
//!
//! ```
//! use repe_core::RepeStruct;
//!
//! #[derive(serde::Serialize, serde::Deserialize, RepeStruct)]
//! struct Build {
//!     version: String,
//!     revision: u64,
//! }
//! ```
//!
//! [`repe`]: https://docs.rs/repe
//! [`RepeMethods`]: structs::RepeMethods

// Lets the derive macros emit `::repe_core` paths that resolve inside this crate
// as well as outside it. See `repe_derive::repe_crate_path`.
extern crate self as repe_core;

pub mod constants;
pub mod structs;

pub use constants::{BodyFormat, ErrorCode, HEADER_SIZE, QueryFormat, REPE_SPEC, REPE_VERSION};
pub use structs::{RepeStruct, ResponseBody, StructError};

/// Derive macro generating a [`structs::RepeStruct`] implementation from a
/// struct's fields.
pub use repe_derive::RepeStruct;

/// Attribute macro that publishes every method of an inherent `impl` block,
/// generating the [`structs::RepeMethods`] table from the signatures themselves.
///
/// Pair it with `#[repe(methods)]` on the `#[derive(RepeStruct)]` struct; each
/// half asserts the other, so neither can be forgotten silently.
pub use repe_derive::methods;

/// Items named by `#[derive(RepeStruct)]`'s generated code. Not public API:
/// nothing outside the derive should name anything here, and its contents may
/// change in any release.
///
/// It exists so generated code reaches `serde_json` through the crate that
/// *defines* the trait rather than through the deriving crate's own dependency
/// list. That buys two things. A deriving crate no longer has to declare
/// `serde_json` for paths nothing in its source mentions — which matters most
/// to exactly the light-dependency crate `repe-core` was split out for. And
/// `RepeStruct`'s signatures name `serde_json::Value`, so the emitted impl must
/// use the *same* `serde_json` the trait was declared with; resolving through
/// here makes that structural instead of a coincidence of everyone being on
/// `serde_json` 1.x, and it keeps working for a crate that renames the
/// dependency (`json = { package = "serde_json" }`), which the old absolute
/// `::serde_json` path did not.
#[doc(hidden)]
pub mod __private {
    pub use serde_json;
}
