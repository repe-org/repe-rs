//! REPE (Remote Efficient Protocol Extension) - Rust implementation
//!
//! Supports JSON, UTF-8, raw binary, and BEVE body formats.
//! Spec reference: <https://github.com/beve-org/beve>

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
pub mod constants;
pub mod error;
#[cfg(not(target_arch = "wasm32"))]
pub mod fleet;
pub mod header;
#[cfg(not(target_arch = "wasm32"))]
pub mod io;
pub mod json_pointer;
pub mod message;
pub mod registry;
#[cfg(not(target_arch = "wasm32"))]
mod server_request;
pub mod server;
pub mod structs;
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub mod udp_client;
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub mod uniudp_fleet;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub mod websocket_client;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub mod websocket_server;
#[cfg(all(feature = "websocket-wasm", target_arch = "wasm32"))]
pub mod wasm_client;

#[doc(hidden)]
pub mod derive {
    pub use repe_derive::RepeStruct;
}

/// Derive macro to generate [`structs::RepeStruct`](crate::structs::RepeStruct) implementations.
pub use repe_derive::RepeStruct;

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
pub use io::{read_message, write_message};
pub use json_pointer::{evaluate as eval_json_pointer, parse as parse_json_pointer};
pub use message::Message;
pub use registry::{Registry, RegistryCallable, RegistryError};
pub use server::{
    IntoTypedResponse, JsonTypedHandler, LockError, Lockable, Middleware, Next, Router,
    TypedResponse,
};
#[cfg(not(target_arch = "wasm32"))]
pub use server::Server;
pub use structs::{RepeStruct, StructError};
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub use udp_client::UniUdpClient;
#[cfg(all(feature = "fleet-udp", not(target_arch = "wasm32")))]
pub use uniudp_fleet::{SendResult, UniUdpFleet, UniUdpNode, UniUdpNodeConfig};
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use websocket_client::WebSocketClient;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use websocket_server::{WebSocketServer, proxy_connection};
#[cfg(all(feature = "websocket-wasm", target_arch = "wasm32"))]
pub use wasm_client::WasmClient;
