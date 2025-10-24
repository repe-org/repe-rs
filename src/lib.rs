//! REPE (Remote Efficient Protocol Extension) - Rust implementation
//!
//! Supports JSON, UTF-8, raw binary, and BEVE body formats.
//! Spec reference: <https://github.com/beve-org/beve>

pub mod async_client;
pub mod async_io;
pub mod async_server;
pub mod client;
pub mod constants;
pub mod error;
pub mod header;
pub mod io;
pub mod json_pointer;
pub mod message;
pub mod server;
pub mod structs;

#[doc(hidden)]
pub mod derive {
    pub use repe_derive::RepeStruct;
}

/// Derive macro to generate [`structs::RepeStruct`](crate::structs::RepeStruct) implementations.
pub use repe_derive::RepeStruct;

pub use async_client::AsyncClient;
pub use async_server::AsyncServer;
pub use client::Client;
pub use constants::{BodyFormat, ErrorCode, QueryFormat, HEADER_SIZE, REPE_SPEC, REPE_VERSION};
pub use error::RepeError;
pub use header::Header;
pub use io::{read_message, write_message};
pub use json_pointer::{evaluate as eval_json_pointer, parse as parse_json_pointer};
pub use message::Message;
pub use server::{
    IntoTypedResponse, JsonTypedHandler, LockError, Lockable, Router, Server, TypedResponse,
};
pub use structs::{RepeStruct, StructError};
