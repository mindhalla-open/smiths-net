//! Sidecar plugin host.
//!
//! Spawns a plugin executable as a child process, speaks JSON-RPC 2.0
//! over its stdin/stdout (newline-delimited frames), and exposes a
//! request/response API. Logs from the plugin's stderr are forwarded
//! to the engine's tracing with a `plugin=<name>` tag.
//!
//! We deliberately use **JSON, not Protobuf**, for the v1 wire: the
//! AI plugin ecosystem is Python/Node-heavy where the cost of pulling
//! in `protoc` is higher than the wire-size win. The
//! `05-ai-plugin-protocol.md` spec notes this choice. Moving to
//! Protobuf later is a `WireFormat` implementation swap, not a
//! protocol change.

// Slice 1.7 lint tightening.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod error;
pub mod rpc;
pub mod sandbox;
pub mod supervisor;

pub use error::Error;
pub use rpc::{RpcRequest, RpcResponse};
pub use supervisor::{PluginNotification, RestartPolicy, Sidecar};
