//! REPE (Remote Efficient Protocol Extension) - Rust implementation
//!
//! Supports JSON, UTF-8, raw binary, and BEVE body formats.
//! Spec reference: <https://github.com/beve-org/beve>

// Lets the derive macros emit `::repe` paths that resolve inside this crate as
// well as outside it. See `repe_derive::repe_crate_path`.
extern crate self as repe;

#[cfg(not(target_arch = "wasm32"))]
pub mod async_client;
#[cfg(not(target_arch = "wasm32"))]
pub mod async_fleet;
#[cfg(not(target_arch = "wasm32"))]
pub mod async_io;
#[cfg(not(target_arch = "wasm32"))]
pub mod async_server;
#[cfg(not(target_arch = "wasm32"))]
pub mod client;
pub mod error;
#[cfg(not(target_arch = "wasm32"))]
pub mod fleet;
pub mod header;
#[cfg(not(target_arch = "wasm32"))]
pub mod io;
pub mod json_pointer;
pub mod message;
// `wasm_client`'s notify slot, hoisted so the host test suite can exercise it.
// Compiled off wasm32 only under `test`; a plain host build has no user for it.
#[cfg(all(feature = "websocket-wasm", any(target_arch = "wasm32", test)))]
mod notify_slot;
pub mod peer;
#[cfg(all(feature = "plugin", not(target_arch = "wasm32")))]
pub mod plugin;
pub mod registry;
#[cfg(all(feature = "rest", not(target_arch = "wasm32")))]
pub mod rest;
pub mod server;
#[cfg(not(target_arch = "wasm32"))]
mod server_request;
#[cfg(not(target_arch = "wasm32"))]
pub mod stream;
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub mod udp_client;
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub mod uniudp_fleet;
#[cfg(all(feature = "value-stream", not(target_arch = "wasm32")))]
pub mod value_stream;
#[cfg(all(feature = "websocket-wasm", target_arch = "wasm32"))]
pub mod wasm_client;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub mod websocket_client;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub mod websocket_limits;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub mod websocket_server;

/// The `RepeStruct` surface, re-exported from [`repe_core`] at the paths it has
/// always had.
///
/// The trait lives in its own crate so a type can be *declared* served without
/// its crate depending on the server, the client, or the transport — a
/// pure-logic crate with a no-I/O charter is the case that forced it. Nothing
/// here moved as far as a caller is concerned: `repe::structs::RepeStruct` and
/// `repe::constants::ErrorCode` name what they always did, and
/// `#[derive(RepeStruct)]` resolves against whichever of the two crates is in
/// scope.
pub use repe_core::{constants, structs};

#[doc(hidden)]
pub mod derive {
    pub use repe_derive::{RepeStruct, methods};
}

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
    // Forwarded from `repe-core` rather than re-exported from this crate's own
    // `serde_json`. `RepeStruct`'s signatures name `repe_core`'s
    // `serde_json::Value`, so generated code has to name that one — and `repe`
    // re-exporting its own would be the same crate only because both manifests
    // say `"1"` and Cargo unifies them. That is a coincidence, narrower than the
    // one this module removed but the same kind. This makes it structural: there
    // is only ever one `serde_json` in play, and it is the trait's.
    pub use repe_core::__private::serde_json;
}

/// Derive macro to generate [`structs::RepeStruct`] implementations.
pub use repe_derive::RepeStruct;

/// Attribute macro that exports a [`server::Router`] constructor as a REPE
/// C-ABI plugin, generating the five symbols a host resolves after `dlopen`.
///
/// Lives in the macro namespace, so `#[repe::plugin(..)]` names this while
/// `repe::plugin::..` names the [`plugin`](mod@crate::plugin) module holding the
/// ABI types.
/// See that module for the buffer contract and deployment requirements.
#[cfg(all(feature = "plugin", not(target_arch = "wasm32")))]
pub use repe_derive::plugin;

/// Attribute macro that publishes every method of an inherent `impl` block,
/// generating the [`structs::RepeMethods`] table from the signatures themselves.
///
/// Pair it with `#[repe(methods)]` on the `#[derive(RepeStruct)]` struct; each
/// half asserts the other, so neither can be forgotten silently.
pub use repe_derive::methods;

#[cfg(not(target_arch = "wasm32"))]
pub use async_client::AsyncClient;
#[cfg(not(target_arch = "wasm32"))]
pub use async_fleet::AsyncFleet;
#[cfg(not(target_arch = "wasm32"))]
pub use async_server::AsyncServer;
#[cfg(not(target_arch = "wasm32"))]
pub use client::Client;
pub use constants::{BodyFormat, ErrorCode, HEADER_SIZE, QueryFormat, REPE_SPEC, REPE_VERSION};
pub use error::RepeError;
#[cfg(not(target_arch = "wasm32"))]
pub use fleet::{
    ConnectSummary, DisconnectSummary, Fleet, FleetError, FleetOptions, HealthStatus, Node,
    NodeConfig, ReconnectSummary, RemoteResult, RetryPolicy,
};
pub use header::Header;
#[cfg(not(target_arch = "wasm32"))]
pub use io::{
    read_message, read_message_into, write_message, write_message_complex_slice,
    write_message_streaming, write_message_typed_slice,
};
pub use json_pointer::{evaluate as eval_json_pointer, parse as parse_json_pointer};

/// Re-exported from `beve` for the typed-numeric body fast path: the element
/// trait bound for [`MessageBuilder::body_typed_slice`] /
/// [`Message::decode_typed_slice`] and the complex element type. Lets callers
/// use the numeric body API without naming `beve` directly.
///
/// [`MessageBuilder::body_typed_slice`]: crate::message::MessageBuilder::body_typed_slice
/// [`Message::decode_typed_slice`]: crate::message::Message::decode_typed_slice
pub use beve::{BeveTypedSlice, Complex};
pub use message::{Message, MessageView};
pub use peer::{
    CallContext, NotifyBody, PeerHandle, PeerId, PeerRegistry, PeerSendError, PeerSink,
};
pub use registry::{Registry, RegistryCallable, RegistryError, WithContext};
#[cfg(not(target_arch = "wasm32"))]
pub use server::Server;
pub use server::{
    Execution, IntoTypedResponse, JsonTypedHandler, LockError, Lockable, Middleware, Next, Router,
    TypedResponse,
};
#[cfg(not(target_arch = "wasm32"))]
pub use stream::{
    CreditError, DEFAULT_BACKPRESSURE_TIMEOUT, DEFAULT_IDLE_TIMEOUT, DEFAULT_RECONNECT_TIMEOUT,
    DEFAULT_REPLAY_RING_BYTES, DEFAULT_WINDOW_BYTES, PendingResume, ReconnectOutcome,
    ResumeRejection, RingChunk, TransferControl, TransferRegistry, spawn_watchdog,
};
pub use structs::{RepeStruct, ResponseBody, StructError};
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub use udp_client::UniUdpClient;
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub use uniudp_fleet::{SendResult, UniUdpFleet, UniUdpNode, UniUdpNodeConfig};
#[cfg(all(feature = "value-stream", not(target_arch = "wasm32")))]
pub use value_stream::{
    AsyncSvsClient, Compression, RouterValueStreamExt, StreamOpts, StreamOutput,
    pull_complex_slice, pull_complex_slice_async, pull_consume, pull_consume_async, pull_stream,
    pull_to_beve_file, pull_to_beve_zst_file, pull_to_file, pull_to_file_async,
    pull_to_file_trailer_verified, pull_to_file_trailer_verified_async,
    pull_to_file_verified_async, pull_to_vec, pull_to_vec_async, pull_typed_slice,
    pull_typed_slice_async, pull_value, pull_value_async,
};
// Both notify-capable clients share this, so it is exported off the feature
// rather than off either transport.
#[cfg(any(feature = "websocket", feature = "websocket-wasm"))]
pub use error::AlreadySubscribed;
#[cfg(all(feature = "websocket-wasm", target_arch = "wasm32"))]
pub use wasm_client::WasmClient;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use websocket_client::WebSocketClient;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use websocket_limits::{DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_MESSAGE_SIZE, WebSocketLimits};
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use websocket_server::{
    ConnectionError, HandshakeContext, SharedWebSocketServer, ShutdownToken, WebSocketServer,
    derive_accept_key, is_websocket_upgrade, proxy_connection,
};
// The `tokio-tungstenite` this crate is built against, so an embedder handing
// repe a connection its own HTTP stack upgraded can name `WebSocketStream` (or
// the `http` types behind `HandshakeContext::from_http_request`) at the matching
// version instead of guessing which requirement to add.
//
// Deliberately a plain comment: rustdoc renders a bare `pub use <crate>;` as a
// Re-exports table row with no description slot, so a doc comment here would not
// reach a reader. The user-facing explanation lives on
// `SharedWebSocketServer::adopt_upgraded` and in `docs/websocket.md`.
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use tokio_tungstenite;
