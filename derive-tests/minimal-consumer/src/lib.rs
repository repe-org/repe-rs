//! A crate that derives `RepeStruct` and declares only what its own source
//! names. See `Cargo.toml` for what is deliberately absent and why.
//!
//! Deliberately exercises every emitter that has ever hardcoded a codec path: a
//! leaf field, a nested child, a whole-object listing, a `#[repe(get)]`
//! accessor, and a method taking an argument (which decodes a body).
//!
//! Note the two declarations per type. `#[derive(RepeStruct)]` publishes the
//! endpoints; `structio::object!` gives the type its wire encoding. They are
//! separate on purpose — a type can have an encoding without being served, and
//! the declaration is a `macro_rules!` macro, so it can be written for a type
//! from a crate you do not own.

use repe_core::RepeStruct;

#[derive(Default, RepeStruct)]
#[repe(methods)]
pub struct Build {
    pub version: String,
    pub revision: u64,
    #[repe(nested)]
    pub child: Child,
    pub tags: Vec<String>,
}
structio::object!(Build {
    version,
    revision,
    child,
    tags
});

#[derive(Default, RepeStruct)]
pub struct Child {
    pub n: u64,
}
structio::object!(Child { n });

#[repe_core::methods]
impl Build {
    /// A field-shaped endpoint: read through the `RepeMethods` accessor path.
    #[repe(get = "slug")]
    fn slug(&self) -> String {
        format!("{version}-{revision}", version = self.version, revision = self.revision)
    }

    /// Takes an argument, so the generated arm deserializes a body.
    fn bump(&mut self, by: u64) -> u64 {
        self.revision += by;
        self.revision
    }

    /// `&self` with an argument: served through the shared borrow.
    fn tagged(&self, prefix: String) -> Vec<String> {
        self.tags.iter().map(|t| format!("{prefix}{t}")).collect()
    }
}
