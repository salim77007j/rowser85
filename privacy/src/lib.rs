//! Rrowser privacy: the network-layer defense stack.
//!
//! * [`blocklist`] — request blocking via Brave's `adblock` engine with the
//!   built-in tracker ruleset (plus optional external lists).
//! * [`cname`] — CNAME-cloaking detection (tracker domains hidden behind
//!   first-party CNAMEs).
//! * [`fingerprint`] — anti-fingerprinting: deterministic, per-session
//!   spoofing profiles for canvas/audio/font/navigator surfaces.
//! * [`safe_browsing`] — privacy-preserving local hash-prefix checks.
//! * [`psl`] — the embedded Public Suffix List for site partitioning.

pub mod blocklist;
pub mod cname;
pub mod fingerprint;
pub mod psl;
pub mod safe_browsing;

/// Global privacy settings, applied by the engine to every request and
/// script environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacySettings {
    /// Network-layer ad/tracker blocking.
    pub block_ads: bool,
    /// Upgrade http:// to https:// where possible.
    pub https_upgrade: bool,
    /// Reject third-party unpartitioned cookies (CHIPS stays enabled).
    pub block_third_party_cookies: bool,
    /// Enable anti-fingerprinting spoofing.
    pub anti_fingerprinting: bool,
    /// WebRTC IP leak protection.
    pub webrtc_protection: bool,
    /// Local safe-browsing checks (no URL ever leaves the device).
    pub safe_browsing: bool,
    /// Telemetry opt-in (off by default; the engine ships zero telemetry).
    pub telemetry_opt_in: bool,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        PrivacySettings {
            block_ads: true,
            https_upgrade: true,
            block_third_party_cookies: true,
            anti_fingerprinting: true,
            webrtc_protection: true,
            safe_browsing: true,
            telemetry_opt_in: false,
        }
    }
}

/// Classification result for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestVerdict {
    /// Allow the request.
    Allow,
    /// Block the request, with a short reason for the UI.
    Block(String),
}
