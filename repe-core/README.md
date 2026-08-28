# repe-core

The `RepeStruct` surface of [`repe`](https://crates.io/crates/repe), on its own: the trait a served struct implements, the error type it reports, the response body it encodes into, and the protocol constants those two name.

Depend on this instead of `repe` when the crate that *declares* a served type is not the crate that serves it. A pure-logic crate — no I/O, no `unsafe`, buildable on any host — can derive `RepeStruct` on its types from here without acquiring a server, a client, or a transport. The crate that actually runs the RPC depends on `repe`, which re-exports everything here, so the two halves name the same trait.

```toml
[dependencies]
repe-core = "1"
```

```rust
use repe_core::RepeStruct;

#[derive(serde::Serialize, serde::Deserialize, RepeStruct)]
pub struct Build {
    pub version: String,
    pub revision: u64,
}
```

Everything in this crate is re-exported by `repe` at the same paths (`repe::structs::*`, `repe::constants::*`), so a type derived against `repe-core` mounts on a `repe::Router` with nothing in between.

See the [`repe` documentation](https://repe-org.github.io/repe-rs/) for the server, the client, and the protocol.
