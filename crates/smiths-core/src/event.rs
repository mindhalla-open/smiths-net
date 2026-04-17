//! Typed event set flowing across the engine's internal bus.
//!
//! The bus is the single mechanism for cross-module communication.
//! Modules publish variants of [`Event`] and subscribe to a broadcast
//! receiver; they never call each other directly.
//!
//! Phase 0 only defines system lifecycle events. Further variants (SIP,
//! media, control, plugin) will be added as their subsystems land.

/// Top-level event envelope.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Event {
    /// Engine-wide lifecycle signals.
    System(SystemEvent),
}

/// Lifecycle signals emitted by the main runtime.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SystemEvent {
    /// All subsystems initialized; the engine is accepting traffic.
    Ready,
    /// A shutdown request has been received (signal or explicit trigger).
    ShutdownRequested,
    /// All subsystems drained; the process is about to exit.
    ShutdownComplete,
}
