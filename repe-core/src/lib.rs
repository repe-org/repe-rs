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
