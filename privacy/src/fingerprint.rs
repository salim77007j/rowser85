//! Anti-fingerprinting: deterministic, per-session spoof profiles.
//!
//! Strategy (matching the 2026 best practice of "randomized but stable"):
//!
//! * Every value is derived from a per-session seed, so a site sees a
//!   consistent fingerprint within the session but a different one across
//!   sessions — breaking cross-site correlation without breaking UX.
//! * Canvas/WebGL readbacks get per-session additive noise.
//! * `navigator` surface is reduced and clamped (hardwareConcurrency,
//!   deviceMemory, platform, userAgent).
//! * Font enumeration is limited to a common-font allowlist.
//! * Audio context outputs get noise comparable to circuit noise.
//!
//! All randomness is derived from blake3 keyed hashing of the session seed
//! (no RNG state, fully reproducible in tests).

use serde::{Deserialize, Serialize};

/// Surfaces the engine spoofs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpoofProfile {
    /// navigator.platform
    pub platform: String,
    /// navigator.userAgent (reduced, brand-minimized)
    pub user_agent: String,
    /// navigator.hardwareConcurrency (clamped 2-8)
    pub hardware_concurrency: u32,
    /// navigator.deviceMemory (always 8)
    pub device_memory: u32,
    /// screen width
    pub screen_width: u32,
    /// screen height
    pub screen_height: u32,
    /// screen color depth
    pub color_depth: u32,
    /// timezone identifier (from the real system; TZ is not fingerprintable
    /// enough to warrant spoofing and spoofing it breaks UX)
    pub timezone: String,
    /// Allowed font families (common set)
    pub allowed_fonts: Vec<String>,
    /// Canvas noise key (per-session).
    pub canvas_noise: [u8; 32],
    /// Audio noise key (per-session).
    pub audio_noise: [u8; 32],
    /// WebGL vendor string
    pub webgl_vendor: String,
    /// WebGL renderer string
    pub webgl_renderer: String,
}

/// Common fonts reported to scripts.
const COMMON_FONTS: [&str; 14] = [
    "Arial",
    "Arial Black",
    "Courier New",
    "Georgia",
    "Impact",
    "Times New Roman",
    "Trebuchet MS",
    "Verdana",
    "Helvetica",
    "Tahoma",
    "Calibri",
    "Cambria",
    "Consolas",
    "Monaco",
];

impl SpoofProfile {
    /// Derives the profile from a 32-byte session seed.
    pub fn from_seed(seed: [u8; 32]) -> SpoofProfile {
        let pick = |context: &str, mod_max: u32| -> u32 {
            let hash = blake3::derive_key(context, &seed);
            let value = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]);
            if mod_max == 0 {
                0
            } else {
                value % mod_max
            }
        };
        let hardware_concurrency = 2 + pick("hardware-concurrency", 7); // 2..=8
        let screen_width = 1280 + 64 * pick("screen-width", 13); // 1280..2048
        let screen_height = 720 + 32 * pick("screen-height", 11); // 720..1040
        let webgl_vendor = if pick("webgl-vendor", 2) == 0 {
            "Google Inc. (Rrowser)"
        } else {
            "Mozilla (Rrowser)"
        };
        let webgl_renderer = "ANGLE (Rrowser, Rrowser SW Renderer, OpenGL)";
        let canvas_noise = *blake3::derive_key("canvas-noise", &seed)
            .first_chunk()
            .unwrap();
        let audio_noise = *blake3::derive_key("audio-noise", &seed)
            .first_chunk()
            .unwrap();

        SpoofProfile {
            platform: "Win32".to_owned(),
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Rrowser/1.0 Chrome/140.0.0.0 Safari/537.36".to_owned(),
            hardware_concurrency,
            device_memory: 8,
            screen_width,
            screen_height,
            color_depth: 24,
            timezone: local_timezone(),
            allowed_fonts: COMMON_FONTS.iter().map(|f| f.to_string()).collect(),
            canvas_noise,
            audio_noise,
            webgl_vendor: webgl_vendor.to_owned(),
            webgl_renderer: webgl_renderer.to_owned(),
        }
    }

    /// Applies per-session noise to canvas readback pixels.
    ///
    /// The noise is deterministic per (seed, channel, position) so repeated
    /// readbacks of identical content return identical values within the
    /// session, while differing from other sessions.
    pub fn noised_canvas_bytes(&self, pixels: &mut [u8]) {
        for (i, px) in pixels.iter_mut().enumerate() {
            let key = blake3::derive_key("canvas", &self.canvas_noise);
            let bucket = key[i % key.len()] as u32;
            // ±2 units of noise, deterministic per byte position.
            let noise = (bucket % 5) as i32 - 2;
            *px = (*px as i32 + noise).clamp(0, 255) as u8;
        }
    }

    /// Returns noise for one audio sample (deterministic per index).
    pub fn audio_noise_at(&self, index: u64) -> f32 {
        let key = blake3::derive_key("audio", &self.audio_noise);
        let bucket = u32::from_le_bytes([
            key[(index % key.len() as u64) as usize],
            key[((index + 1) % key.len() as u64) as usize],
            key[((index + 2) % key.len() as u64) as usize],
            key[((index + 3) % key.len() as u64) as usize],
        ]);
        // ±0.0001: below hearing threshold, above fingerprint stability.
        ((bucket % 200) as f32 - 100.0) / 1_000_000.0
    }
}

fn local_timezone() -> String {
    // Use the system's IANA timezone; jiff reads the process environment.
    jiff::tz::TimeZone::system()
        .iana_name()
        .unwrap_or("UTC")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_within_session() {
        let profile = SpoofProfile::from_seed([7u8; 32]);
        let mut a = vec![100u8; 64];
        let mut b = vec![100u8; 64];
        profile.noised_canvas_bytes(&mut a);
        profile.noised_canvas_bytes(&mut b);
        assert_eq!(a, b, "noise must be deterministic within a session");
        assert_ne!(a, vec![100u8; 64], "noise must actually perturb pixels");
    }

    #[test]
    fn differs_across_sessions() {
        let p1 = SpoofProfile::from_seed([1u8; 32]);
        let p2 = SpoofProfile::from_seed([2u8; 32]);
        let mut a = vec![128u8; 64];
        let mut b = vec![128u8; 64];
        p1.noised_canvas_bytes(&mut a);
        p2.noised_canvas_bytes(&mut b);
        assert_ne!(a, b);
        assert_ne!(p1.canvas_noise, p2.canvas_noise);
    }

    #[test]
    fn clamped_values() {
        let profile = SpoofProfile::from_seed([9u8; 32]);
        assert!((2..=8).contains(&profile.hardware_concurrency));
        assert_eq!(profile.device_memory, 8);
        assert!(profile.screen_width >= 1280 && profile.screen_width <= 2048);
        assert!(!profile.allowed_fonts.is_empty());
        let n = profile.audio_noise_at(0);
        assert!(n.abs() < 0.0002);
    }
}
