//! A crate that derives `RepeStruct` and declares only what its own source
//! names. See `Cargo.toml` for what is deliberately absent and why.
//!
//! Deliberately exercises every emitter that has ever hardcoded a `serde_json`
//! path: a leaf field, a nested child, a whole-object listing, a `#[repe(get)]`
//! accessor, and a method taking an argument (which deserializes a body).

use repe_core::RepeStruct;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize, RepeStruct)]
#[repe(methods)]
pub struct Build {
    pub version: String,
    pub revision: u64,
    #[repe(nested)]
    pub child: Child,
    pub tags: Vec<String>,
}

#[derive(Default, Serialize, Deserialize, RepeStruct)]
pub struct Child {
    pub n: u64,
}

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
