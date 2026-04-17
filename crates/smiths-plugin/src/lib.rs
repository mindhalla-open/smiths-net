//! Plugin manifest, registry, and hook dispatcher.
//!
//! Tier-agnostic: the dispatcher treats WASM and sidecar plugins
//! identically, routing to `smiths-wasm` or `smiths-sidecar` based on
//! each plugin's manifest.
//!
//! **Phase 0 stub.** Phase 3 lands here.
