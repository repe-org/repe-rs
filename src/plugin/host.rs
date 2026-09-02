//! The other side of the plugin ABI: load a plugin and drive it.
//!
//! [`Plugin`] is the safe wrapper around the five symbols
//! [`plugin`](mod@crate::plugin) exports. It resolves them, performs the version
//! handshake **before** reading anything whose layout the version governs, reads
//! the metadata, runs the optional initializer, and turns each
//! `repe_plugin_call` into an owned `Vec<u8>` — copying the response inside the
//! call, which is the whole reason this type exists.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use repe::plugin::host::Plugin;
//!
//! // SAFETY: loading a native library runs its initializers; this one is a
//! // plugin built against `glaze/rpc/repe/plugin.h`.
//! let plugin = unsafe { Plugin::load("libinstrument.so") }?;
//! println!("{} {} claims {}", plugin.name(), plugin.version(), plugin.root_path());
//!
//! if let Some(response) = plugin.call(&request_frame)? {
//!     // `response` is owned. Nothing borrows across the boundary.
//! }
//! # Ok(()) }
//! # const request_frame: [u8; 0] = [];
//! ```
//!
//! # What this layer is, and what it deliberately is not
//!
//! This is the loader and the call wrapper: generic, and where all the unsafe
//! lives. It holds **one** plugin.
//!
//! Everything above it is deployment policy rather than protocol, and is left to
//! the application: which directory plugins are read from, which file extensions
//! count, whether a reload probes before replacing, how a request path is
//! matched against a set of claimed roots, and whether any of that is published
//! as an RPC surface. Those answers differ per deployment, and none of them can
//! be got right here on a caller's behalf.
//!
//! Routing is the seam between the two. A host dispatches to a plugin with
//! [`Plugin::claims`], against a query that
//! [`MessageView`] reads out of a frame without
//! decoding the body.
//!
//! # The library is never unloaded
//!
//! [`load`](Plugin::load) leaks its handle, and dropping a [`Plugin`] does not
//! unload anything.
//!
//! That is not a shortcut, it is the only correct behavior available. A plugin's
//! response buffer is a thread-local; on glibc, unloading a library while
//! threads that touched its TLS are still alive leaves destructor addresses
//! pointing into unmapped memory, and the resulting crash lands at thread exit,
//! arbitrarily far from the unload that caused it. A host that hot-reloads
//! plugins runs straight into this, because the natural implementation unloads
//! the old library.
//!
//! Two consequences worth planning for rather than discovering:
//!
//! * **Reloading a path yields the loaded copy, not the new file.** `dlopen`
//!   refcounts by path, so re-loading a library that is already resident hands
//!   back the resident one. A host that replaces a plugin binary in place and
//!   reloads it will serve the old code with no error anywhere. Publish each
//!   build under its own path if reload has to mean anything — and where the
//!   path is not the host's to choose, [`Plugin::load_origin`] says which of the
//!   two happened, so a reload endpoint handed the wrong path can report a no-op
//!   instead of a success.
//! * **Two [`Plugin`] values for one path are two handles to one instance.**
//!   They share the plugin's state, its initialization, and its shutdown.
//!
//! # Concurrency
//!
//! [`Plugin`] is [`Send`] and [`Sync`], and `plugin.h` permits concurrent calls
//! from multiple threads, each with its own response buffer. Sharing one
//! `Plugin` across a thread pool is the intended use.
//!
//! Whether the *work* runs concurrently is the plugin's business. A plugin built
//! on [`with_struct`](crate::server::Router::with_struct) puts its value behind a
//! mutex, so calls against it serialize however many threads the host runs.
//!
//! # What a host still depends on the plugin for
//!
//! A panic or an exception that escapes `repe_plugin_call` cannot be contained
//! here: it unwinds through an `extern "C"` frame, which aborts. This crate's own
//! plugins guard against it, and `docs/plugins.md` records the build setting that
//! keeps that guard live (`panic = "unwind"`), but a host cannot verify either
//! from the outside.

use core::ffi::{CStr, c_char};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use libloading::Library;

use crate::constants::ErrorCode;
use crate::error::RepeError;
use crate::message::{Message, MessageView, create_error_response_for};
use crate::peer::CallContext;
use crate::server::HandlerErased;

use super::{REPE_PLUGIN_INTERFACE_VERSION, RepeBuffer, RepePluginData, RepeResult};

/// The oldest plugin ABI version this host can drive.
const MIN_SUPPORTED_INTERFACE_VERSION: u32 = 3;

/// An inverted range accepts nothing, so a host built on it would refuse every
/// plugin including the ones this crate ships. Caught here rather than in a
/// deployment.
const _: () = assert!(MIN_SUPPORTED_INTERFACE_VERSION <= REPE_PLUGIN_INTERFACE_VERSION);

/// The plugin ABI versions [`Plugin::load`] accepts.
///
/// A **range** rather than the exact equality `plugin.h` recommends, because the
/// two ends of this ABI are not symmetric. A plugin exports whatever version it
/// was compiled against and has no choice about it; a host can reasonably drive
/// an older plugin. Under exact equality every ABI bump orphans every plugin
/// binary in a deployment on the same day, which is a cost the operator pays for
/// a change neither end asked for.
///
/// It is `3..=3` today, because 3 is the only layout this crate has ever bound.
/// What the range buys is the *next* bump: an additive one leaves the lower
/// bound where it is, and every plugin binary already deployed keeps loading.
///
/// A deployment with a stricter policy — "this host takes exactly the version it
/// was built against" — can have it by comparing
/// [`interface_version`](Plugin::interface_version) after loading. That is
/// policy, and it belongs with the host that has one.
pub fn supported_interface_versions() -> RangeInclusive<u32> {
    MIN_SUPPORTED_INTERFACE_VERSION..=REPE_PLUGIN_INTERFACE_VERSION
}

/// Why a plugin could not be loaded, or could not be called.
///
/// Every variant is a plugin that failed to hold up its end of
/// `glaze/rpc/repe/plugin.h`, or a library that is not a plugin at all. Errors
/// *within* a well-formed exchange do not appear here: a handler that failed, an
/// unknown method, a malformed request frame are all ordinary REPE error
/// responses, and arrive as `Ok(Some(frame))`.
///
/// `#[non_exhaustive]` because the conditions come from an ABI this crate does
/// not own: a version of `plugin.h` that adds a required symbol or a lifecycle
/// result adds a way to fail with it, and that should not be a major release
/// here.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// The dynamic loader refused the file.
    #[error("could not load plugin library {}: {source}", path.display())]
    Load {
        /// The path handed to [`Plugin::load`].
        path: PathBuf,
        /// The loader's own diagnostic.
        #[source]
        source: libloading::Error,
    },

    /// A required symbol is missing, so this library is not a REPE plugin.
    ///
    /// Only `repe_plugin_interface_version`, `repe_plugin_info` and
    /// `repe_plugin_call` reach this. `repe_plugin_init` and
    /// `repe_plugin_shutdown` are marked optional by `plugin.h`, and their
    /// absence is a valid plugin rather than a fault.
    #[error("plugin library {} does not export `{symbol}`: {source}", path.display())]
    MissingSymbol {
        /// The path handed to [`Plugin::load`].
        path: PathBuf,
        /// The unresolved symbol.
        symbol: &'static str,
        /// The loader's own diagnostic.
        #[source]
        source: libloading::Error,
    },

    /// The plugin speaks an ABI version this host cannot.
    ///
    /// Checked before the metadata struct is read, which is the reason
    /// `repe_plugin_interface_version` is a standalone function rather than a
    /// field: the version is what says whether the struct's layout can be
    /// trusted.
    #[error(
        "plugin library {} reports ABI version {reported}; this host supports {}..={}",
        path.display(),
        supported_interface_versions().start(),
        supported_interface_versions().end()
    )]
    UnsupportedInterfaceVersion {
        /// The path handed to [`Plugin::load`].
        path: PathBuf,
        /// What `repe_plugin_interface_version` returned. What this host accepts
        /// is [`supported_interface_versions`].
        reported: u32,
    },

    /// `repe_plugin_info` returned null, or one of its fields is unusable.
    ///
    /// `plugin.h` gives null the meaning "the host should refuse to load the
    /// plugin", so this is a refusal rather than a plugin with no name.
    #[error("plugin library {} has unusable metadata: {reason}", path.display())]
    Metadata {
        /// The path handed to [`Plugin::load`].
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },

    /// `repe_plugin_init` reported a failure.
    ///
    /// `REPE_ERROR_ALREADY_INITIALIZED` is not one: a plugin may initialize
    /// lazily on its first call, and loading a path twice reaches an instance
    /// that is already up. Both are working plugins.
    #[error("repe_plugin_init in {} failed with code {code}", path.display())]
    InitFailed {
        /// The path handed to [`Plugin::load`].
        path: PathBuf,
        /// The raw `repe_result` the plugin returned.
        code: i32,
    },

    /// `repe_plugin_call` returned a null `data` with a non-zero `size`.
    ///
    /// There is nothing to read at a null address, so this is refused rather
    /// than served. A `size` of zero is *not* an error whatever `data` holds —
    /// that is the "no response" a notify produces.
    #[error("repe_plugin_call returned a null data pointer with size {size}")]
    NullResponse {
        /// The size the plugin claimed for a buffer it did not supply.
        size: u64,
    },

    /// `repe_plugin_call` claimed a response larger than this target can address.
    ///
    /// A 64-bit length on a 32-bit host reaches this, and so does a length that
    /// exceeds `isize::MAX` — the limit on any single Rust allocation, and on
    /// the slice that would have to be formed to read the response.
    #[error("repe_plugin_call returned {size} bytes, more than this target can address")]
    ResponseTooLarge {
        /// The size the plugin reported.
        size: u64,
    },
}

/// What a [`Plugin::load`] did to the library it names.
///
/// `dlopen` refcounts by path, so a load either maps the library or reaches a
/// copy the process already had. Both produce a working [`Plugin`], and without
/// this they are indistinguishable in every observable one has — which is how a
/// hot-reload endpoint comes to report success for a load that read no file.
///
/// `#[non_exhaustive]` because the set of answers is the loader's, not this
/// crate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadOrigin {
    /// The load mapped the library: the file was read and, if the plugin exports
    /// one, its initializer ran for the first time.
    Mapped,
    /// The library was already resident. No file was read, no initializer ran,
    /// and a binary rebuilt in place at that path was not picked up.
    ///
    /// Not an error: the handle works, and it shares the plugin's state and
    /// initialization with every other handle to the same path. It is the *load*
    /// that was a no-op, which is the thing a host needs to be able to say.
    AlreadyResident,
    /// This platform's dynamic loader cannot be asked.
    ///
    /// The probe is `RTLD_NOLOAD`, which POSIX does not require and which
    /// OpenBSD, Haiku, AIX and VxWorks do not provide. Everywhere this crate is
    /// built and tested — Linux, macOS, the BSDs that have the flag, and Windows
    /// via `GetModuleHandleExW` — answers [`Mapped`](Self::Mapped) or
    /// [`AlreadyResident`](Self::AlreadyResident).
    ///
    /// A host should report it as "cannot tell" rather than folding it into
    /// either answer; that is the whole reason it is a variant instead of a
    /// convenient default.
    Unknown,
}

/// `repe_plugin_interface_version`.
type InterfaceVersionFn = unsafe extern "C" fn() -> u32;
/// `repe_plugin_info`.
type InfoFn = unsafe extern "C" fn() -> *const RepePluginData;
/// `repe_plugin_init`.
///
/// Declared as returning `i32` rather than [`RepeResult`], deliberately.
/// `repe_result` is a plain C enum, so its ABI type is `int`, and a plugin that
/// returns a value outside the three named codes — a hand-written C plugin
/// forwarding an errno, say — would be undefined behavior the instant it
/// materialized as a Rust enum with no such variant. Taking the raw `int` and
/// classifying it here keeps a non-conforming plugin a *diagnosable* fault
/// instead of an unsound one.
type InitFn = unsafe extern "C" fn() -> i32;
/// `repe_plugin_shutdown`.
type ShutdownFn = unsafe extern "C" fn();
/// `repe_plugin_call`.
type CallFn = unsafe extern "C" fn(*const c_char, u64) -> RepeBuffer;

/// The five entry points, resolved.
///
/// Split out so [`Plugin::from_abi`] — the version handshake, the metadata read,
/// and the initializer, which is all of the protocol — is reachable in tests
/// without a shared library on disk. [`Plugin::load`] is then only the loader.
struct PluginAbi {
    interface_version: InterfaceVersionFn,
    info: InfoFn,
    init: Option<InitFn>,
    shutdown: Option<ShutdownFn>,
    call: CallFn,
}

/// A loaded REPE plugin.
///
/// Holds no borrows: the metadata is copied in at load time and every response
/// is copied out before it is returned, so nothing here is invalidated by a
/// later call. See the [module docs](self) for the lifetime of the underlying
/// library, which is the process.
pub struct Plugin {
    path: PathBuf,
    origin: LoadOrigin,
    interface_version: u32,
    name: String,
    version: String,
    root_path: String,
    call: CallFn,
    shutdown: Option<ShutdownFn>,
}

/// A real address to hand across for an empty request.
///
/// The mirror of the plugin side's non-null empty buffer, and for the same
/// reason: a C++ plugin that builds a `std::string_view` from a null pointer is
/// undefined behavior even at length zero, and the pointer of an empty Rust
/// slice is non-null but not a real object. An empty request is not valid REPE
/// and the plugin will say so — this is about it being able to say so.
static EMPTY_REQUEST_ANCHOR: u8 = 0;

impl Plugin {
    /// Load the plugin at `path` and bring it up.
    ///
    /// In order: `dlopen`, resolve the three required symbols and the two
    /// optional ones, check the ABI version, read the metadata, and call
    /// `repe_plugin_init` if the plugin exports it. The returned `Plugin` is
    /// ready to serve.
    ///
    /// Note that a failure here still leaves the library loaded — see the
    /// [module docs](self) for why nothing is ever unloaded. A rejected plugin
    /// costs address space until the process exits, which is the price of not
    /// unmapping code whose thread-locals may already have been touched.
    ///
    /// # Safety
    ///
    /// Loading a native library executes code that this crate cannot inspect:
    /// the platform runs the library's initializers during `dlopen`, before any
    /// symbol is resolved or any version is checked. The caller is responsible
    /// for the provenance of the file.
    ///
    /// Beyond that, the library must implement `glaze/rpc/repe/plugin.h`: the
    /// symbols must have the signatures declared there, `repe_plugin_info` must
    /// return either null or a struct of nul-terminated strings that live as
    /// long as the library, and `repe_plugin_call` must return a buffer valid
    /// until the calling thread's next call. It must also honor the header's
    /// concurrency clause — `repe_plugin_call` callable from several threads at
    /// once, each with its own response buffer — which is what licenses
    /// [`Plugin`] being [`Sync`].
    ///
    /// Everything this crate *can* check about that contract, it checks; a
    /// library that lies about a signature cannot be checked from here at all.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, HostError> {
        let path = path.as_ref().to_path_buf();

        // Before the open, because after it the answer is always "resident".
        // This is what [`load_origin`](Self::load_origin) reports, and the only
        // point at which the distinction is observable.
        let origin = probe_origin(&path);

        let library = match unsafe { open(&path) } {
            Ok(library) => library,
            Err(source) => return Err(HostError::Load { path, source }),
        };

        // Resolving a symbol only looks a name up; nothing here interprets a
        // layout, so it is safe to do before the version check. Reading what
        // `repe_plugin_info` *points at* is not, and does not happen until
        // `from_abi` has cleared the version.
        //
        // Every `?` is deferred past the `forget` below, deliberately. An early
        // return with `library` still live would drop it, and dropping a
        // `Library` calls `dlclose` — on a library whose initializers have
        // already run, which is the one thing this module says must never
        // happen. A rejected plugin costs address space until the process
        // exits; that is the price of not unmapping code whose thread-locals
        // may already have been touched.
        let interface_version = required(&library, &path, "repe_plugin_interface_version");
        let info = required(&library, &path, "repe_plugin_info");
        let init = optional(&library, "repe_plugin_init");
        let shutdown = optional(&library, "repe_plugin_shutdown");
        let call = required(&library, &path, "repe_plugin_call");

        // Past this point the function pointers must outlive `library`, so the
        // handle is leaked rather than dropped. `forget` is how that is spelled.
        std::mem::forget(library);

        let abi = PluginAbi {
            interface_version: interface_version?,
            info: info?,
            init,
            shutdown,
            call: call?,
        };

        // SAFETY: every pointer above came from `dlsym` on this library, which
        // is now resident for the life of the process.
        unsafe { Self::from_abi(path, abi, origin) }
    }

    /// The version handshake, the metadata read, and initialization.
    ///
    /// # Safety
    ///
    /// The function pointers in `abi` must be valid for the life of the process
    /// and must have the signatures `plugin.h` declares.
    unsafe fn from_abi(
        path: PathBuf,
        abi: PluginAbi,
        origin: LoadOrigin,
    ) -> Result<Self, HostError> {
        // First, and before anything whose meaning depends on it. `plugin.h` is
        // explicit that the version governs whether the metadata struct's layout
        // can be trusted, which is why it is a standalone function.
        //
        // SAFETY: caller's obligation. Takes no arguments and returns a `u32`,
        // so there is nothing here to get wrong beyond the signature itself.
        let interface_version = unsafe { (abi.interface_version)() };
        if !supported_interface_versions().contains(&interface_version) {
            return Err(HostError::UnsupportedInterfaceVersion {
                path,
                reported: interface_version,
            });
        }

        // SAFETY: caller's obligation, and the version above says this layout is
        // the one the plugin wrote.
        let info = unsafe { (abi.info)() };
        if info.is_null() {
            return Err(HostError::Metadata {
                path,
                reason: "repe_plugin_info returned NULL".to_string(),
            });
        }
        // SAFETY: non-null, and `plugin.h` requires the pointee to outlive the
        // library. Copied out by value, so nothing borrows the plugin's struct.
        let info = unsafe { *info };

        // SAFETY: each field is a nul-terminated string valid for the plugin's
        // lifetime, per `plugin.h`. Null is checked inside.
        let name = unsafe { string_field(info.name, "name", &path) }?;
        let version = unsafe { string_field(info.version, "version", &path) }?;
        let root_path = unsafe { string_field(info.root_path, "root_path", &path) }?;

        // A host matches request queries against this prefix, so a root no query
        // could ever match is a plugin that loads, reports healthy, and answers
        // method-not-found for everything under it. Much better caught at load,
        // where the plugin's name is still in hand.
        //
        // Two shapes are refused, the same two `#[repe::plugin]` refuses on the
        // exporting side: relative (`instrument`), which no absolute query
        // starts with, and a trailing separator (`/instrument/`), which is not
        // part of the prefix.
        //
        // Empty is *accepted*, and looks like it should not be. Glaze's registry
        // defaults its root to `""` when an object is published at the top
        // level, so a C++ plugin built the idiomatic way reports it and refusing
        // it would reject a plugin `plugin.h` permits. Such a plugin claims the
        // whole namespace, which [`claims`](Self::claims) handles and a host
        // loading more than one plugin has to account for.
        if !root_path.is_empty() && !root_path.starts_with('/') {
            return Err(HostError::Metadata {
                path,
                reason: format!(
                    "root_path {root_path:?} is not an absolute JSON Pointer prefix: it must start with `/`"
                ),
            });
        }
        if root_path.len() > 1 && root_path.ends_with('/') {
            return Err(HostError::Metadata {
                path,
                reason: format!(
                    "root_path {root_path:?} ends with `/`, which no request query it is matched against will"
                ),
            });
        }

        if let Some(init) = abi.init {
            // SAFETY: caller's obligation. `plugin.h` says init is called once
            // before any call, which is where this is.
            let code = unsafe { init() };
            // `AlreadyInitialized` is a working plugin, not a failure: a plugin
            // may build itself lazily on first call, and a second `load` of the
            // same path reaches the instance the first one brought up.
            if code != RepeResult::Ok as i32 && code != RepeResult::AlreadyInitialized as i32 {
                return Err(HostError::InitFailed { path, code });
            }
        }

        Ok(Self {
            path,
            origin,
            interface_version,
            name,
            version,
            root_path,
            call: abi.call,
            shutdown: abi.shutdown,
        })
    }

    /// The path this plugin was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this [`load`](Self::load) mapped the library, reached a copy the
    /// process already had resident, or could not be asked. See [`LoadOrigin`]
    /// for what each answer means, and the [module docs](self) for why the
    /// library is never unloaded and what a deployment does about it.
    ///
    /// Do not confuse [`LoadOrigin::AlreadyResident`] with the ABI's
    /// `REPE_ERROR_ALREADY_INITIALIZED`: a plugin that initializes lazily
    /// returns that on a genuine first load, so it answers a different question
    /// and answers this one wrongly.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use repe::plugin::host::{LoadOrigin, Plugin};
    ///
    /// // SAFETY: a plugin built against `glaze/rpc/repe/plugin.h`.
    /// let plugin = unsafe { Plugin::load("libinstrument.so") }?;
    /// match plugin.load_origin() {
    ///     LoadOrigin::Mapped => println!("loaded {}", plugin.name()),
    ///     // Report the reload as a no-op rather than as a success.
    ///     LoadOrigin::AlreadyResident => println!(
    ///         "{} was already resident; the file on disk was not read",
    ///         plugin.path().display()
    ///     ),
    ///     _ => println!("this platform cannot say whether the file was read"),
    /// }
    /// # Ok(()) }
    /// ```
    pub fn load_origin(&self) -> LoadOrigin {
        self.origin
    }

    /// The ABI version the plugin reported, which is within
    /// [`supported_interface_versions`].
    pub fn interface_version(&self) -> u32 {
        self.interface_version
    }

    /// The plugin's name, from `repe_plugin_data::name`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The plugin's own version string, from `repe_plugin_data::version`.
    ///
    /// Unrelated to [`interface_version`](Self::interface_version): this one is
    /// the plugin's release, the other is the ABI it speaks.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The RPC path prefix this plugin claims, from
    /// `repe_plugin_data::root_path`.
    ///
    /// Absolute and free of a trailing separator, or empty — which is what a
    /// plugin published at the top level reports, and means it claims
    /// everything. Use [`claims`](Self::claims) to match a query against it
    /// rather than comparing prefixes by hand.
    pub fn root_path(&self) -> &str {
        &self.root_path
    }

    /// Whether `query` names this plugin's object or something beneath it.
    ///
    /// This is how a host decides a request belongs to this plugin, and it is
    /// not `query.starts_with(root_path)`: under that test a plugin rooted at
    /// `/inst` swallows every request meant for one rooted at `/instrument`. The
    /// separator has to be part of the comparison, and it is the sort of thing
    /// that works in every test a host writes and fails once two plugins are
    /// deployed whose roots share a prefix.
    ///
    /// A plugin with an empty root claims every absolute query.
    pub fn claims(&self, query: &str) -> bool {
        match query.strip_prefix(self.root_path.as_str()) {
            // The object itself, e.g. a whole-object read of `/instrument`.
            Some("") => true,
            // Something under it. The separator must be there, not merely the
            // characters of the root.
            Some(rest) => rest.starts_with('/'),
            None => false,
        }
    }

    /// Dispatch one REPE request frame and return the response.
    ///
    /// `Ok(None)` means the plugin produced no response, which is what a notify
    /// request does. It is not an error, and a host must send nothing rather
    /// than report something.
    ///
    /// A request the plugin rejects — an unknown method, a malformed frame, a
    /// handler that failed — comes back as `Ok(Some(frame))` carrying a REPE
    /// error response. Those belong to the caller that sent the request, not to
    /// the host, and are forwarded rather than interpreted.
    pub fn call(&self, request: &[u8]) -> Result<Option<Vec<u8>>, HostError> {
        let mut response = Vec::new();
        if self.call_into(request, &mut response)? {
            Ok(Some(response))
        } else {
            Ok(None)
        }
    }

    /// [`call`](Self::call) into a caller-owned buffer, returning whether a
    /// response was written.
    ///
    /// `response` is cleared first, and its capacity is reused, so a host that
    /// keeps one buffer per connection or per worker thread does not allocate
    /// per request. `false` is the notify case, and leaves the buffer empty.
    pub fn call_into(&self, request: &[u8], response: &mut Vec<u8>) -> Result<bool, HostError> {
        response.clear();

        let data = if request.is_empty() {
            &raw const EMPTY_REQUEST_ANCHOR
        } else {
            request.as_ptr()
        };

        // SAFETY: `data` is a real address for `request.len()` readable bytes —
        // the slice's, or the anchor's for the empty case, where the length is
        // zero. `request` is borrowed for the whole call and the plugin is not
        // permitted to retain it.
        let buffer = unsafe { (self.call)(data.cast::<c_char>(), request.len() as u64) };

        // Zero size is "no response" whatever `data` holds. Checked before the
        // pointer, because a notify is a normal outcome, and `plugin.h` says
        // nothing about `data` when `size` is 0 — a null there is only out of
        // *this crate's* stricter buffer contract, and nothing is read through
        // it, so it is not something to fail a host over.
        if buffer.size == 0 {
            return Ok(false);
        }

        // Two bounds. `usize::try_from` catches a 64-bit size on a 32-bit host;
        // `isize::MAX` is the limit on the slice that is about to be formed, and
        // on any Rust allocation, which a 64-bit host would otherwise sail past.
        let size = match usize::try_from(buffer.size) {
            Ok(size) if size <= isize::MAX as usize => size,
            _ => return Err(HostError::ResponseTooLarge { size: buffer.size }),
        };
        if buffer.data.is_null() {
            return Err(HostError::NullResponse { size: buffer.size });
        }

        // The copy the ABI requires, made here so no caller has to know that it
        // was required. The borrowed buffer dies at this thread's next call, and
        // this is the only place a reference to it ever exists.
        //
        // SAFETY: non-null and `size` readable bytes, per the contract checked
        // above. `u8` has no alignment requirement, and the slice does not
        // outlive this statement, let alone the next call.
        response.extend_from_slice(unsafe {
            std::slice::from_raw_parts(buffer.data.cast::<u8>(), size)
        });
        Ok(true)
    }

    /// Shut the plugin down, if it exports `repe_plugin_shutdown`.
    ///
    /// Consumes the handle, because `plugin.h` states that no further calls will
    /// be made afterward and a `Plugin` exists to make calls. A plugin that
    /// exports no shutdown hook needs no cleanup, and this is then only a drop.
    ///
    /// Dropping a `Plugin` *without* calling this does not shut the plugin down.
    /// That is deliberate. The library is never unloaded, so shutdown is a
    /// one-way latch on an instance that outlives every handle to it, and it
    /// cannot be undone: reloading the path reaches the same retired instance,
    /// and for a plugin built on this crate its initializer then refuses, so
    /// [`load`](Self::load) fails with [`InitFailed`](HostError::InitFailed). A
    /// plugin that latches nothing simply stays shut down. Any handle taken
    /// *before* the
    /// shutdown keeps working in the sense that it still gets answers — they are
    /// error frames. Tying that latch to a Rust value's lifetime would let an
    /// ordinary early return retire the plugin for the rest of the process.
    pub fn shutdown(self) {
        if let Some(shutdown) = self.shutdown {
            // SAFETY: resolved from a resident library, and `self` is consumed,
            // so no further call can be made through this handle.
            unsafe { shutdown() };
        }
    }
}

/// Serve a plugin's endpoints from a [`Router`], which is what mounting one on
/// [`Router::with_fallback`] needs.
///
/// The router hands a handler a decoded request and expects a decoded response;
/// the plugin ABI moves whole REPE frames. This impl is that translation, and
/// nothing more — it does not decide *which* plugin a request belongs to, which
/// is the host's table to keep. The usual shape is a fallback handler that owns
/// the table, consults [`claims`](Plugin::claims), and calls through to the
/// plugin it picked; for the single-plugin case the plugin can be the fallback
/// itself, and [`claims`](Plugin::claims) is then what decides whether to answer
/// or to frame a [`MethodNotFound`](ErrorCode::MethodNotFound).
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use repe::plugin::host::Plugin;
/// use repe::server::Router;
/// use std::sync::Arc;
///
/// // SAFETY: loading a native library runs its initializers.
/// let plugin = unsafe { Plugin::load("libinstrument.so") }?;
/// // Typed `Arc`, so the handle can be reclaimed for `shutdown` later; the
/// // clone handed to the router coerces to `Arc<dyn HandlerErased>`.
/// let plugin = Arc::new(plugin);
/// let router = Router::new().with_fallback_blocking(plugin.clone());
/// # let _ = router;
/// # Ok(()) }
/// ```
///
/// [`with_fallback_blocking`](crate::server::Router::with_fallback_blocking)
/// rather than `with_fallback`: `repe_plugin_call` enters a library this process
/// did not build, for an unbounded time, so on the WebSocket server it belongs
/// off the reader task. On the TCP servers the two are the same.
///
/// One consequence of sharing a plugin with a router: [`shutdown`](Plugin::shutdown)
/// consumes the handle. Reclaiming it needs the **typed** `Arc<Plugin>` above —
/// `Arc::try_unwrap` cannot be applied to the `Arc<dyn HandlerErased>` the
/// router holds, because `dyn HandlerErased` is not `Sized`:
///
/// ```no_run
/// # use repe::plugin::host::Plugin;
/// # use repe::server::Router;
/// # use std::sync::Arc;
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let plugin = Arc::new(unsafe { Plugin::load("libinstrument.so") }?);
/// # let router = Router::new().with_fallback_blocking(plugin.clone());
/// drop(router);
/// Arc::try_unwrap(plugin)
///     .expect("the router was the only other holder")
///     .shutdown();
/// # Ok(()) }
/// ```
///
/// A plugin that is meant to live as long as the process needs none of that.
///
/// A request the plugin refuses — an unknown method under its root, a handler
/// that failed — comes back as the plugin's own error frame and is forwarded
/// unchanged. Only a plugin that breaks the ABI, by answering a call with
/// nothing or with a frame that does not parse, is turned into an
/// [`InternalError`](ErrorCode::InternalError) here: the client asked this
/// server a question, and it gets an answer either way.
///
/// [`Router`]: crate::server::Router
/// [`Router::with_fallback`]: crate::server::Router::with_fallback
impl HandlerErased for Plugin {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        if !self.claims(req.query_str().unwrap_or("")) {
            return Ok(self.disclaim(req.header.id, &req.query));
        }
        let mut frame = Vec::with_capacity(req.serialized_len());
        req.write_to(&mut frame)?;
        Ok(self.forward(req.header, &req.query, &frame))
    }

    fn handle_view(&self, view: &MessageView, _ctx: &CallContext) -> Result<Message, RepeError> {
        // The frame has to be reassembled either way — the ABI takes bytes — but
        // it is assembled straight from the borrowed view, so the owned
        // `Message` the default `handle_view` would have materialized first is
        // never built. The result is byte-identical to what `handle` produces:
        // `Header::decode` has already rejected any frame whose `length`
        // disagrees with its query and body lengths, so recomputing them here
        // cannot change them.
        if !self.claims(std::str::from_utf8(view.query).unwrap_or("")) {
            return Ok(self.disclaim(view.header.id, view.query));
        }
        let mut frame =
            Vec::with_capacity(crate::constants::HEADER_SIZE + view.query.len() + view.body.len());
        crate::io::write_message_streaming(
            &mut frame,
            view.header,
            view.query,
            view.body.len() as u64,
            |w| std::io::Write::write_all(w, view.body),
        )?;
        Ok(self.forward(view.header, view.query, &frame))
    }
}

impl Plugin {
    /// Frame the error response for a request whose path this plugin does not
    /// claim.
    ///
    /// The router has already missed everything else by the time a fallback
    /// runs, so there is nobody left to hand the request to. Answering here
    /// costs no FFI call, gives one uniform message, and keeps the reply for a
    /// path this plugin never claimed from depending on a foreign library.
    fn disclaim(&self, id: u64, query: &[u8]) -> Message {
        create_error_response_for(
            id,
            query,
            ErrorCode::MethodNotFound,
            format!("Method not found: {}", String::from_utf8_lossy(query)),
        )
    }

    /// Hand `frame` to the plugin and turn what comes back into a response.
    ///
    /// `header` and `query` are the request's, and are all an error response
    /// takes from it.
    fn forward(&self, header: crate::header::Header, query: &[u8], frame: &[u8]) -> Message {
        let error = |code, message| create_error_response_for(header.id, query, code, message);
        let mut response = Vec::new();
        match self.call_into(frame, &mut response) {
            Ok(true) => match Message::from_slice(&response) {
                Ok(message) => message,
                Err(err) => error(
                    ErrorCode::InternalError,
                    format!(
                        "plugin `{}` answered with a frame that does not parse: {err}",
                        self.name
                    ),
                ),
            },
            // No response. That is correct for a notify, where the caller
            // discards whatever is returned here; for anything else the plugin
            // dropped a request that was awaiting an answer.
            Ok(false) if header.notify == 1 => Message::builder().id(header.id).build(),
            Ok(false) => error(
                ErrorCode::InternalError,
                format!(
                    "plugin `{}` returned no response to a request that expects one",
                    self.name
                ),
            ),
            Err(err) => error(ErrorCode::InternalError, err.to_string()),
        }
    }
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("path", &self.path)
            .field("name", &self.name)
            .field("version", &self.version)
            .field("root_path", &self.root_path)
            .field("interface_version", &self.interface_version)
            .field("has_shutdown", &self.shutdown.is_some())
            .finish()
    }
}

/// `dlopen` the library, resolving every symbol up front on platforms that offer
/// the choice.
///
/// `libloading`'s portable constructor uses `RTLD_LAZY`, which defers an
/// unresolved dependency to the first call that reaches it — a deferred abort
/// with no [`HostError`] anywhere, in a type whose whole job is to turn a
/// plugin's faults into diagnosable errors, and after [`Plugin::load`] has
/// promised a plugin that is ready to serve. `RTLD_NOW` moves that to the load,
/// where it is reportable, and matches what the C++ host in `interop/cpp` does.
///
/// # Safety
///
/// As [`Plugin::load`]: the platform runs the library's initializers here.
unsafe fn open(path: &Path) -> Result<Library, libloading::Error> {
    #[cfg(unix)]
    // SAFETY: the caller's obligation, restated.
    unsafe {
        use libloading::os::unix::{RTLD_LOCAL, RTLD_NOW};
        libloading::os::unix::Library::open(Some(path), RTLD_NOW | RTLD_LOCAL).map(Library::from)
    }
    #[cfg(not(unix))]
    // SAFETY: as above. Windows resolves imports at load time regardless, so
    // there is no equivalent flag to pass.
    unsafe {
        Library::new(path)
    }
}

/// Ask the dynamic loader whether the library at `path` is already resident.
///
/// The probe behind [`Plugin::load_origin`], and the only point at which the
/// question can be asked: after the real open the answer is always yes.
///
/// It takes a reference to the library and releases it again, which is the one
/// `dlclose` this module permits. The invariant is that nothing is ever
/// *unmapped*, not that `dlclose` is never spelled: the library was already
/// resident before the probe, so returning the count to what it was cannot reach
/// zero. A library that is not resident is never opened here at all — that is
/// what `RTLD_NOLOAD` means.
///
/// The `cfg` names the four unix targets whose loader has no `RTLD_NOLOAD`.
/// POSIX does not require the flag, so this is a property of the platform rather
/// than a gap in `libc`, and it is why [`LoadOrigin::Unknown`] exists at all.
#[cfg(all(
    unix,
    not(any(
        target_os = "openbsd",
        target_os = "haiku",
        target_os = "aix",
        target_os = "vxworks"
    ))
))]
fn probe_origin(path: &Path) -> LoadOrigin {
    use libloading::os::unix::{Library, RTLD_LAZY, RTLD_LOCAL};

    // `RTLD_LAZY` rather than `RTLD_NOW`: with `RTLD_NOLOAD` nothing is mapped,
    // so there is nothing to bind, and asking for eager binding on a probe would
    // only give the loader work to refuse. Neither flag promotes anything on an
    // object that is already resident.
    //
    // Only `RTLD_NOLOAD` comes from `libc` — libloading carries the two POSIX
    // flags itself, including a fallback for targets whose values it does not
    // know.
    //
    // SAFETY: `RTLD_NOLOAD` cannot map a library, so no initializer runs here
    // and none of `dlopen`'s usual provenance obligations apply. The handle is
    // dropped without a symbol ever being resolved through it.
    let resident =
        unsafe { Library::open(Some(path), libc::RTLD_NOLOAD | RTLD_LAZY | RTLD_LOCAL) }.is_ok();
    if resident {
        LoadOrigin::AlreadyResident
    } else {
        LoadOrigin::Mapped
    }
}

/// The unix targets with no `RTLD_NOLOAD`. Nothing here can distinguish the two
/// outcomes, and saying so is better than picking one.
#[cfg(all(
    unix,
    any(
        target_os = "openbsd",
        target_os = "haiku",
        target_os = "aix",
        target_os = "vxworks"
    )
))]
fn probe_origin(_path: &Path) -> LoadOrigin {
    LoadOrigin::Unknown
}

/// Windows counterpart of the `RTLD_NOLOAD` probe.
///
/// `GetModuleHandleExW` takes a reference to an already-loaded module and fails
/// rather than loading one, which is the same question; dropping the handle
/// releases that reference, exactly as on Unix. Note that it matches on module
/// name when `path` carries no directory, so a host that wants the answer to be
/// about a specific file should hand [`Plugin::load`] a full path — which is
/// what a deployment publishing each build under its own path does anyway.
#[cfg(not(unix))]
fn probe_origin(path: &Path) -> LoadOrigin {
    if libloading::os::windows::Library::open_already_loaded(path).is_ok() {
        LoadOrigin::AlreadyResident
    } else {
        LoadOrigin::Mapped
    }
}

/// Resolve a symbol `plugin.h` marks required.
fn required<T: Copy>(library: &Library, path: &Path, symbol: &'static str) -> Result<T, HostError> {
    // SAFETY: the caller of `load` warranted that this library implements
    // `plugin.h`, which declares this symbol with the signature `T` names.
    // Copied out of the `Symbol` so the result does not borrow the library.
    match unsafe { library.get::<T>(symbol.as_bytes()) } {
        Ok(symbol) => Ok(*symbol),
        Err(source) => Err(HostError::MissingSymbol {
            path: path.to_path_buf(),
            symbol,
            source,
        }),
    }
}

/// Resolve a symbol `plugin.h` marks optional (`repe_plugin_init`,
/// `repe_plugin_shutdown`).
///
/// Absence is a valid plugin, not a fault: the header says a host that finds no
/// initializer "assumes initialization is handled lazily", and no shutdown hook
/// means no cleanup is needed. So any resolution failure collapses to `None` —
/// there is no reading of it under which the load should stop.
fn optional<T: Copy>(library: &Library, symbol: &'static str) -> Option<T> {
    // SAFETY: as `required`.
    unsafe { library.get::<T>(symbol.as_bytes()) }
        .ok()
        .map(|symbol| *symbol)
}

/// Copy one `repe_plugin_data` string field out of the plugin.
///
/// # Safety
///
/// `field` must be null or a nul-terminated string valid for the plugin's
/// lifetime.
unsafe fn string_field(
    field: *const c_char,
    name: &'static str,
    path: &Path,
) -> Result<String, HostError> {
    if field.is_null() {
        return Err(HostError::Metadata {
            path: path.to_path_buf(),
            reason: format!("repe_plugin_data::{name} is NULL"),
        });
    }
    // SAFETY: non-null and nul-terminated, per the caller's obligation.
    let field = unsafe { CStr::from_ptr(field) };
    field
        .to_str()
        .map(str::to_string)
        .map_err(|_| HostError::Metadata {
            path: path.to_path_buf(),
            reason: format!("repe_plugin_data::{name} is not valid UTF-8"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{ErrorCode, QueryFormat};
    use crate::message::Message;
    use crate::plugin::PluginRuntime;
    use crate::server::Router;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A plugin that behaves, implemented in-process.
    ///
    /// These exercise [`Plugin::from_abi`] and [`Plugin::call_into`] — the
    /// version handshake, the metadata contract, and the buffer copy — against
    /// plugins that misbehave in each individual way a real one could, which is
    /// not something a well-built `cdylib` on disk can be asked to do. The
    /// `dlopen`/`dlsym` half is covered end to end against a real shared library
    /// in `tests/plugin_host.rs`.
    mod good {
        use super::*;

        static RUNTIME: PluginRuntime = PluginRuntime::new(build);

        fn build() -> Router {
            Router::new().with_typed("/example/double", |v: i64| Ok(v * 2))
        }

        pub unsafe extern "C" fn interface_version() -> u32 {
            REPE_PLUGIN_INTERFACE_VERSION
        }

        pub unsafe extern "C" fn info() -> *const RepePluginData {
            static DATA: RepePluginData = RepePluginData {
                name: c"example".as_ptr(),
                version: c"1.0.0".as_ptr(),
                root_path: c"/example".as_ptr(),
            };
            &raw const DATA
        }

        pub unsafe extern "C" fn init() -> i32 {
            RUNTIME.init() as i32
        }

        pub unsafe extern "C" fn shutdown() {
            RUNTIME.shutdown();
        }

        pub unsafe extern "C" fn call(request: *const c_char, size: u64) -> RepeBuffer {
            // SAFETY: the host hands over a live slice for the call's duration.
            unsafe { RUNTIME.call(request, size) }
        }
    }

    fn abi() -> PluginAbi {
        PluginAbi {
            interface_version: good::interface_version,
            info: good::info,
            init: Some(good::init),
            shutdown: Some(good::shutdown),
            call: good::call,
        }
    }

    fn load(abi: PluginAbi) -> Result<Plugin, HostError> {
        // SAFETY: every pointer is a `fn` item in this binary, so it is valid
        // for the life of the process and has the declared signature.
        unsafe { Plugin::from_abi(PathBuf::from("in-process"), abi, LoadOrigin::Mapped) }
    }

    fn request<T: structio::json::Write + ?Sized>(path: &str, body: &T, notify: bool) -> Vec<u8> {
        Message::builder()
            .id(7)
            .notify(notify)
            .query_str(path)
            .query_format(QueryFormat::JsonPointer)
            .body_json(body)
            .build()
            .to_vec()
    }

    #[test]
    fn a_conforming_plugin_loads_and_serves() {
        let plugin = load(abi()).expect("a conforming plugin loads");
        assert_eq!(plugin.name(), "example");
        assert_eq!(plugin.version(), "1.0.0");
        assert_eq!(plugin.root_path(), "/example");
        assert_eq!(plugin.interface_version(), REPE_PLUGIN_INTERFACE_VERSION);

        let response = plugin
            .call(&request("/example/double", &21i64, false))
            .expect("the call crosses the ABI")
            .expect("a non-notify request produces a response");
        let response = Message::from_slice(&response).unwrap();
        assert_eq!(response.error_code(), Some(ErrorCode::Ok));
        assert_eq!(response.body, b"42");
    }

    #[test]
    fn a_notify_produces_no_response() {
        let plugin = load(abi()).expect("a conforming plugin loads");
        let response = plugin
            .call(&request("/example/double", &1i64, true))
            .expect("the call crosses the ABI");
        assert!(
            response.is_none(),
            "a zero-size buffer is `no response`, not an error"
        );
    }

    #[test]
    fn call_into_reuses_the_caller_s_buffer() {
        let plugin = load(abi()).expect("a conforming plugin loads");
        let mut buffer = Vec::new();

        assert!(
            plugin
                .call_into(&request("/example/double", &2i64, false), &mut buffer)
                .unwrap()
        );
        let after_first = buffer.capacity();
        assert!(!buffer.is_empty());

        // A notify clears the buffer and reports that nothing was written,
        // rather than leaving the previous response in it to be sent twice.
        assert!(
            !plugin
                .call_into(&request("/example/double", &2i64, true), &mut buffer)
                .unwrap()
        );
        assert!(buffer.is_empty());
        assert_eq!(buffer.capacity(), after_first, "the allocation is reused");
    }

    #[test]
    fn an_empty_request_is_answered_rather_than_dereferenced() {
        // The pointer of an empty Rust slice is non-null but is not a real
        // object; a plugin that forms a `string_view` from it is on thin ice.
        // The host hands over a real address, so the plugin can do what it does
        // with any unparseable frame: answer.
        let plugin = load(abi()).expect("a conforming plugin loads");
        let response = plugin
            .call(&[])
            .expect("the call crosses the ABI")
            .expect("an empty request is answered, not dropped");
        let response = Message::from_slice(&response).unwrap();
        assert_ne!(response.error_code(), Some(ErrorCode::Ok));
    }

    #[test]
    fn a_handler_error_is_forwarded_rather_than_reported_as_a_host_error() {
        // An unknown method is the caller's problem, not the host's: it comes
        // back as a REPE frame addressed to whoever sent the request.
        let plugin = load(abi()).expect("a conforming plugin loads");
        let response = plugin
            .call(&request("/example/absent", &None::<i64>, false))
            .expect("an unknown method is not a host error")
            .expect("it is answered rather than dropped");
        let response = Message::from_slice(&response).unwrap();
        assert_eq!(response.error_code(), Some(ErrorCode::MethodNotFound));
    }

    #[test]
    fn an_unsupported_interface_version_is_refused_before_the_metadata_is_read() {
        // The metadata function here would abort the test if it were called at
        // all, which is the property `plugin.h` asks a host for: the version is
        // checked *before* anything whose layout it governs.
        unsafe extern "C" fn from_the_future() -> u32 {
            REPE_PLUGIN_INTERFACE_VERSION + 1
        }
        unsafe extern "C" fn unreadable_info() -> *const RepePluginData {
            unreachable!("the metadata must not be read before the version is cleared")
        }

        let error = load(PluginAbi {
            interface_version: from_the_future,
            info: unreadable_info,
            ..abi()
        })
        .expect_err("a version this host does not speak is refused");
        assert!(matches!(
            error,
            HostError::UnsupportedInterfaceVersion { reported, .. }
                if reported == REPE_PLUGIN_INTERFACE_VERSION + 1
        ));
    }

    #[test]
    fn an_empty_root_path_is_accepted_and_claims_everything() {
        // Glaze's registry defaults its root to "" for an object published at
        // the top level, so this is what an idiomatically built C++ plugin
        // reports. Refusing it would reject a plugin the ABI permits.
        unsafe extern "C" fn top_level() -> *const RepePluginData {
            static DATA: RepePluginData = RepePluginData {
                name: c"example".as_ptr(),
                version: c"1.0.0".as_ptr(),
                root_path: c"".as_ptr(),
            };
            &raw const DATA
        }
        let plugin = load(PluginAbi {
            info: top_level,
            ..abi()
        })
        .expect("an empty root is a plugin published at the top level");
        assert_eq!(plugin.root_path(), "");
        assert!(plugin.claims("/gain"));
        assert!(plugin.claims("/anything/at/all"));
    }

    #[test]
    fn claims_puts_the_separator_in_the_comparison() {
        let plugin = load(abi()).expect("a conforming plugin loads");
        assert_eq!(plugin.root_path(), "/example");

        assert!(plugin.claims("/example"), "the object itself");
        assert!(plugin.claims("/example/double"), "something under it");

        // The trap this method exists for: under a bare `starts_with`, a plugin
        // rooted at `/example` swallows every request meant for one rooted at
        // `/example_two`, and it only shows up once both are deployed.
        assert!(!plugin.claims("/example_two/double"));
        assert!(!plugin.claims("/examples"));
        assert!(!plugin.claims("/other/double"));
        assert!(!plugin.claims("example/double"), "not absolute");
    }

    #[test]
    fn null_metadata_is_a_refusal() {
        unsafe extern "C" fn no_info() -> *const RepePluginData {
            std::ptr::null()
        }
        let error = load(PluginAbi {
            info: no_info,
            ..abi()
        })
        .expect_err("`plugin.h` gives NULL the meaning `refuse to load`");
        assert!(matches!(error, HostError::Metadata { .. }));
    }

    #[test]
    fn a_null_metadata_field_is_a_refusal_rather_than_a_dereference() {
        unsafe extern "C" fn null_name() -> *const RepePluginData {
            static DATA: RepePluginData = RepePluginData {
                name: std::ptr::null(),
                version: c"1.0.0".as_ptr(),
                root_path: c"/example".as_ptr(),
            };
            &raw const DATA
        }
        let error = load(PluginAbi {
            info: null_name,
            ..abi()
        })
        .expect_err("a null field cannot be read");
        assert!(matches!(error, HostError::Metadata { reason, .. } if reason.contains("name")));
    }

    #[test]
    fn a_root_path_that_could_never_match_is_refused_at_load() {
        // Both of these route to nothing and do it silently, which is the worst
        // available failure mode: the plugin loads, reports healthy, and every
        // request under it comes back method-not-found.
        unsafe extern "C" fn relative_root() -> *const RepePluginData {
            static DATA: RepePluginData = RepePluginData {
                name: c"example".as_ptr(),
                version: c"1.0.0".as_ptr(),
                root_path: c"example".as_ptr(),
            };
            &raw const DATA
        }
        unsafe extern "C" fn trailing_separator() -> *const RepePluginData {
            static DATA: RepePluginData = RepePluginData {
                name: c"example".as_ptr(),
                version: c"1.0.0".as_ptr(),
                root_path: c"/example/".as_ptr(),
            };
            &raw const DATA
        }

        for (info, why) in [
            (relative_root as InfoFn, "relative"),
            (trailing_separator as InfoFn, "trailing separator"),
        ] {
            let error = load(PluginAbi { info, ..abi() })
                .err()
                .unwrap_or_else(|| panic!("a {why} root_path is refused"));
            assert!(
                matches!(error, HostError::Metadata { ref reason, .. } if reason.contains("root_path")),
                "a {why} root_path reports what is wrong with it, got {error}"
            );
        }
    }

    #[test]
    fn a_failed_initializer_stops_the_load() {
        unsafe extern "C" fn refuses() -> i32 {
            RepeResult::InitFailed as i32
        }
        let error = load(PluginAbi {
            init: Some(refuses),
            ..abi()
        })
        .expect_err("a plugin that cannot initialize is not usable");
        assert!(matches!(error, HostError::InitFailed { code, .. } if code == 1));
    }

    #[test]
    fn an_already_initialized_plugin_is_not_a_failure() {
        // What a second `load` of one path sees, and what a lazily-initializing
        // plugin reports. Both are working plugins.
        unsafe extern "C" fn already() -> i32 {
            RepeResult::AlreadyInitialized as i32
        }
        load(PluginAbi {
            init: Some(already),
            ..abi()
        })
        .expect("ALREADY_INITIALIZED is a plugin that is up");
    }

    #[test]
    fn an_unrecognized_init_code_is_reported_rather_than_transmuted() {
        // The reason `InitFn` returns `i32` and not `RepeResult`: materializing
        // this as an enum with no such variant would be undefined behavior.
        unsafe extern "C" fn errno_ish() -> i32 {
            13
        }
        let error = load(PluginAbi {
            init: Some(errno_ish),
            ..abi()
        })
        .expect_err("an unknown result code is not success");
        assert!(matches!(error, HostError::InitFailed { code, .. } if code == 13));
    }

    #[test]
    fn a_plugin_with_no_lifecycle_hooks_loads_and_serves() {
        // `plugin.h` marks both optional, so their absence is a valid plugin.
        let plugin = load(PluginAbi {
            init: None,
            shutdown: None,
            ..abi()
        })
        .expect("init and shutdown may be absent");
        assert!(
            plugin
                .call(&request("/example/double", &3i64, false))
                .unwrap()
                .is_some()
        );
        // And shutting down a plugin with no hook is a no-op rather than a panic.
        plugin.shutdown();
    }

    #[test]
    fn a_null_response_pointer_is_refused_rather_than_read() {
        unsafe extern "C" fn lies(_: *const c_char, _: u64) -> RepeBuffer {
            RepeBuffer {
                data: std::ptr::null(),
                size: 32,
            }
        }
        let plugin = load(PluginAbi {
            call: lies,
            ..abi()
        })
        .unwrap();
        assert!(matches!(
            plugin.call(&request("/example/double", &1i64, false)),
            Err(HostError::NullResponse { size: 32 })
        ));
    }

    #[test]
    fn a_zero_size_response_is_a_notify_even_with_a_null_pointer() {
        // `plugin.h` says nothing about `data` at `size == 0`, and nothing is
        // read through it. A host that failed here would reject a plugin over a
        // detail no caller can observe.
        unsafe extern "C" fn empty_and_null(_: *const c_char, _: u64) -> RepeBuffer {
            RepeBuffer {
                data: std::ptr::null(),
                size: 0,
            }
        }
        let plugin = load(PluginAbi {
            call: empty_and_null,
            ..abi()
        })
        .unwrap();
        assert!(
            plugin
                .call(&request("/example/double", &1i64, false))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_unreadable_response_size_is_refused_rather_than_sliced() {
        // `from_raw_parts` is undefined past `isize::MAX`, so this must be
        // rejected before the slice is formed rather than after.
        unsafe extern "C" fn too_big(_: *const c_char, _: u64) -> RepeBuffer {
            RepeBuffer {
                data: (&raw const EMPTY_REQUEST_ANCHOR).cast::<c_char>(),
                size: u64::MAX,
            }
        }
        let plugin = load(PluginAbi {
            call: too_big,
            ..abi()
        })
        .unwrap();
        assert!(matches!(
            plugin.call(&request("/example/double", &1i64, false)),
            Err(HostError::ResponseTooLarge { size: u64::MAX })
        ));
    }

    // ---------------------------------------------------------------------
    // `HandlerErased` — the plugin mounted on a router
    // ---------------------------------------------------------------------

    /// Drive the mounted plugin the way a carrier does: through
    /// [`Router::call`], which exercises the borrowing `handle_view` path.
    fn through_router(plugin: Plugin, request: &[u8]) -> Option<Message> {
        let router = Router::new().with_fallback(Arc::new(plugin));
        router
            .call(request)
            .map(|frame| Message::from_slice(&frame).expect("a REPE frame comes back"))
    }

    #[test]
    fn a_mounted_plugin_serves_what_it_claims() {
        let response = through_router(
            load(abi()).unwrap(),
            &request("/example/double", &21i64, false),
        )
        .expect("a non-notify request is answered");
        assert_eq!(response.json_body::<i64>().unwrap(), 42);
        assert_eq!(response.header.id, 7);
    }

    #[test]
    fn a_mounted_plugin_declines_a_path_outside_its_root() {
        // The router has already missed everything else by the time a fallback
        // runs, so a path the plugin does not claim has nobody left to serve it.
        // Answering is the only thing that does not strand the caller.
        let response = through_router(load(abi()).unwrap(), &request("/elsewhere", &1i64, false))
            .expect("a declined path is still answered");
        assert_eq!(response.error_code(), Some(ErrorCode::MethodNotFound));
        assert!(
            response
                .error_message_utf8()
                .is_some_and(|text| text.contains("/elsewhere"))
        );
    }

    #[test]
    fn a_mounted_plugin_forwards_a_notify_without_answering() {
        // `is_none()` alone would pass even if the plugin were never called:
        // the dispatch layer discards a notify's response whatever the handler
        // did. So the plugin records the call and the test reads it back.
        static SEEN: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn counting(request: *const c_char, size: u64) -> RepeBuffer {
            SEEN.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the host hands over a live slice for the call's duration.
            unsafe { good::call(request, size) }
        }

        let plugin = load(PluginAbi {
            call: counting,
            ..abi()
        })
        .unwrap();
        let before = SEEN.load(Ordering::SeqCst);
        assert!(through_router(plugin, &request("/example/double", &1i64, true),).is_none());
        assert_eq!(
            SEEN.load(Ordering::SeqCst),
            before + 1,
            "the notify reached the plugin rather than being dropped at the mount"
        );
    }

    #[test]
    fn a_plugin_that_answers_nothing_to_a_real_request_is_an_internal_error() {
        // Silence is correct for a notify and a broken promise for anything
        // else. A host that passed it through would hang the caller, so the
        // ABI breach is named where it happened.
        unsafe extern "C" fn silent(_: *const c_char, _: u64) -> RepeBuffer {
            RepeBuffer {
                data: std::ptr::null(),
                size: 0,
            }
        }

        let plugin = load(PluginAbi {
            call: silent,
            ..abi()
        })
        .unwrap();
        let response = through_router(plugin, &request("/example/double", &1i64, false))
            .expect("the caller is told rather than left waiting");
        assert_eq!(response.error_code(), Some(ErrorCode::InternalError));
    }

    #[test]
    fn a_plugin_answer_that_is_not_a_frame_is_an_internal_error() {
        unsafe extern "C" fn garbage(_: *const c_char, _: u64) -> RepeBuffer {
            static JUNK: &[u8] = b"not a repe frame";
            RepeBuffer {
                data: JUNK.as_ptr().cast::<c_char>(),
                size: JUNK.len() as u64,
            }
        }

        let plugin = load(PluginAbi {
            call: garbage,
            ..abi()
        })
        .unwrap();
        let response =
            through_router(plugin, &request("/example/double", &1i64, false)).expect("answered");
        assert_eq!(response.error_code(), Some(ErrorCode::InternalError));
    }

    #[test]
    fn a_mounted_plugin_s_own_error_frame_is_forwarded_unchanged() {
        // The distinction the declining test above cannot make: a request the
        // plugin *claims* and then refuses is answered by the plugin, and that
        // frame belongs to whoever sent the request. The host forwards it
        // rather than replacing it with one of its own.
        //
        // A bodiless request to a `with_json` route is the clearest case: the
        // plugin answers `InvalidBody`, which the host has no way to produce —
        // it frames only `MethodNotFound` and `InternalError`.
        let bodiless = Message::builder()
            .id(7)
            .query_str("/example/double")
            .query_format(QueryFormat::JsonPointer)
            .build()
            .to_vec();
        let response = through_router(load(abi()).unwrap(), &bodiless).expect("answered");
        assert_eq!(response.error_code(), Some(ErrorCode::InvalidBody));
        assert_eq!(response.header.id, 7);
    }

    #[test]
    fn the_owning_and_borrowing_dispatch_paths_agree() {
        // `handle` frames from an owned `Message`, `handle_view` from a borrowed
        // view; both have to produce the same answer, and only one of them is on
        // the path `Router::call` takes.
        let plugin = load(abi()).unwrap();
        let frame = request("/example/double", &21i64, false);
        let owned = Message::from_slice(&frame).unwrap();
        let view = MessageView::from_slice(&frame).unwrap();

        let by_message = plugin.handle(&owned).unwrap();
        let by_view = plugin
            .handle_view(&view, &CallContext::detached("/example/double"))
            .unwrap();
        assert_eq!(by_message.body, by_view.body);
        assert_eq!(by_message.header.ec, by_view.header.ec);
        assert_eq!(by_message.header.id, by_view.header.id);
    }

    #[test]
    fn a_plugin_is_send_and_sync() {
        // `plugin.h` permits concurrent calls, and a host shares one handle
        // across its worker threads. Holding no borrows is what makes that safe,
        // so this pins the property rather than assuming it survives a field
        // being added.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Plugin>();
    }
}
