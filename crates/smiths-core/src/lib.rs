//! Core runtime primitives for smiths-net.
//!
//! This crate hosts everything every other module depends on:
//! configuration, the typed event bus, graceful shutdown, and the shared
//! error type. It pulls in no sibling workspace crates — it is the root
//! of the dependency graph.

pub mod bus;
pub mod config;
pub mod error;
pub mod event;
pub mod shutdown;

pub use bus::EventBus;
pub use config::{Config, CoreConfig, LogFormat, ObservabilityConfig};
pub use error::Error;
pub use event::{Event, SystemEvent};
pub use shutdown::Shutdown;
