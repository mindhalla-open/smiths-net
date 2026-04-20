//! `[ice]` config section.
//!
//! Narrow on purpose — ICE has a dozen knobs it could expose, but
//! MVP only needs an on/off switch and an optional STUN-server list
//! (used by a later slice for server-reflexive candidates). The
//! engine's `[ice]` section lives on [`smiths_core::Config`] once the
//! CLI wire-up slice lands; today this crate just defines the typed
//! form.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// `[ice]` TOML block.
///
/// ```toml
/// [ice]
/// enabled = true
/// stun_servers = ["stun.l.google.com:19302"]
/// ```
///
/// When `enabled = false` (the default today) the engine skips ICE
/// entirely and relies on direct SDP-advertised RTP addresses. That's
/// the MVP posture — host candidates only, no server-reflexive — so
/// operators who don't need NAT traversal pay nothing.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IceConfig {
    /// Master enable for ICE. `false` = engine bypasses ICE and uses
    /// plain SDP addressing (MVP pre-1.4 behaviour). `true` = gather
    /// host candidates and run connectivity checks before RTP flows.
    pub enabled: bool,
    /// STUN servers to query for server-reflexive candidates. Empty
    /// = host-only (MVP scope). When non-empty a later slice's
    /// gatherer will fire a Binding Request at each, and emit an
    /// `srflx` candidate if the response lands.
    pub stun_servers: Vec<SocketAddr>,
}

impl IceConfig {
    /// `true` if ICE gathering should run at all.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        let cfg = IceConfig::default();
        assert!(!cfg.is_on());
        assert!(cfg.stun_servers.is_empty());
    }

    #[test]
    fn deserializes_from_toml() {
        let text = r#"
            enabled = true
            stun_servers = ["1.2.3.4:3478", "[::1]:3478"]
        "#;
        let cfg: IceConfig = toml::from_str(text).unwrap();
        assert!(cfg.is_on());
        assert_eq!(cfg.stun_servers.len(), 2);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = r#"
            enabled = true
            mystery = "trickle"
        "#;
        assert!(toml::from_str::<IceConfig>(text).is_err());
    }
}
