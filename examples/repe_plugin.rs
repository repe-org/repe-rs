//! A REPE plugin: a `cdylib` that any REPE host can `dlopen` and drive.
//!
//! Build it with
//!
//! ```text
//! cargo build --release --features plugin --example repe_plugin
//! ```
//!
//! which leaves a `librepe_plugin.so` (`.dylib` on macOS) in `target/release/examples`.
//! A host resolves five symbols from it and speaks ordinary REPE frames across
//! the boundary — including the C++ host in Glaze, since both ends implement the
//! same ABI from `glaze/rpc/repe/plugin.h`.
//!
//! The shape here is the one this ABI exists for: a device-control object whose
//! **fields** are readable and writable state and whose **methods** are commands,
//! published wholesale by reflecting off the type. Nothing below restates the RPC
//! surface — the derive produces it from the struct, and `#[repe::plugin]`
//! produces the C exports from the router.
//!
//! In a real plugin crate this file would be `src/lib.rs` with
//! `crate-type = ["cdylib"]` in its own `Cargo.toml`; it lives in `examples/`
//! here so the repository builds and link-checks it on every CI run.

use repe::server::Router;

/// The object this plugin publishes. Every field becomes a readable and
/// writable endpoint under the plugin's root; every method in the `impl` block
/// below becomes a callable one.
#[derive(Default, repe::RepeStruct)]
#[repe(methods)]
struct Instrument {
    /// Read-write: `/instrument/gain`.
    gain: f64,
    /// Read-write: `/instrument/channel`.
    channel: u32,
    /// Read-only, because the device reports it and a client setting it would
    /// be meaningless.
    #[repe(readonly)]
    firmware: String,
    /// `#[repe(typed)]` sends the samples as a BEVE typed array — one bulk copy
    /// rather than an element-by-element JSON walk.
    #[repe(typed)]
    samples: [f64; 8],
}
structio::object!(Instrument { gain, channel, firmware, samples });

/// A command failed on the device. Any `Display` error works; the crate turns it
/// into a REPE error response.
#[derive(Debug)]
struct InstrumentError(&'static str);

impl std::fmt::Display for InstrumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "instrument fault: {}", self.0)
    }
}

#[repe::methods]
impl Instrument {
    /// `/instrument/identify` — a read, taking `&self`.
    fn identify(&self) -> String {
        format!(
            "instrument fw {} on channel {}",
            self.firmware, self.channel
        )
    }

    /// `/instrument/calibrate` — a command that mutates, taking `&mut self`.
    fn calibrate(&mut self, reference: f64) -> Result<f64, InstrumentError> {
        if reference <= 0.0 {
            return Err(InstrumentError("reference must be positive"));
        }
        self.gain = reference / 2.0;
        Ok(self.gain)
    }

    /// `/instrument/reset` — no arguments, no result.
    fn reset(&mut self) {
        self.gain = 1.0;
        self.samples = [0.0; 8];
    }
}

/// Build the router this plugin serves.
///
/// `#[repe::plugin]` turns it into the five C exports. `name` and `version`
/// default to this crate's `CARGO_PKG_NAME` and `CARGO_PKG_VERSION`, so the
/// plugin's identity comes from the manifest that already carries it and cannot
/// drift from it.
///
/// The function stays callable, which is the point: the same router that goes
/// over the ABI can be driven directly by an in-process test, with no `dlopen`
/// and no host in the loop.
///
/// `with_struct_rw` is the part worth copying. `plugin.h` permits a host to call
/// `repe_plugin_call` from several threads at once, and `with_struct`'s default
/// `Mutex` would serialize every one of them — a `/gain` read included. Behind
/// the `RwLock` this registers, reads (`identify` and every field) share the
/// guard; only `calibrate` and `reset`, which take `&mut self`, take it
/// exclusively.
#[repe::plugin(root = "/instrument")]
fn build() -> Router {
    let instrument = Instrument {
        gain: 1.0,
        channel: 0,
        firmware: "1.4.2".to_string(),
        samples: [0.0; 8],
    };
    Router::new().with_struct_rw("/instrument", instrument).0
}
