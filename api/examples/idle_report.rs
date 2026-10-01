//! Idle memory + startup reporter for browser comparisons.
//!
//! Boots the engine, opens N tabs against a URL (about:blank by default),
//! waits until settled, then prints machine-readable metrics:
//!
//! ```text
//! BOOT_MS=412 CPU_SECONDS=0.9 TABS=8 RSS_KB=84000
//! ```
//!
//! Usage: `idle_report [tabs] [settle_seconds] [url]`

use std::time::{Duration, Instant};

use rowser_api::BrowserApi;
use rowser_engine::EngineConfig;

fn rss_kb() -> u64 {
    // /proc/self/status works on Linux; fallback reports 0 on other OSes.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse()
                    .unwrap_or(0);
            }
        }
    }
    0
}

fn cpu_seconds() -> f64 {
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        let fields: Vec<&str> = stat.split_whitespace().collect();
        // utime(14) + stime(15) in clock ticks (100/s on Linux).
        if fields.len() > 15 {
            let utime: f64 = fields[13].parse().unwrap_or(0.0);
            let stime: f64 = fields[14].parse().unwrap_or(0.0);
            return (utime + stime) / 100.0;
        }
    }
    0.0
}

fn main() {
    let tabs: usize = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let settle: u64 = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let url = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "about:blank".into());

    let start = Instant::now();
    let profile = std::env::temp_dir().join(format!("rowser-idle-{}", std::process::id()));
    let config = EngineConfig {
        profile_dir: profile.clone(),
        ..EngineConfig::default()
    };
    let browser = BrowserApi::start(config).expect("engine start");
    let boot_ms = start.elapsed().as_millis();

    for _ in 0..tabs {
        browser.new_tab(Some(url.clone()));
    }
    // Give in-flight navigations time to finish, and the suspension sweep
    // time to freeze background tabs: the report reflects the suspended
    // steady state (idle RAM), matching how idling browsers are measured.
    std::thread::sleep(Duration::from_secs(settle));

    println!(
        "BOOT_MS={boot_ms} TABS={tabs} CPU_SECONDS={:.3} RSS_KB={}",
        cpu_seconds(),
        rss_kb()
    );
    browser.shutdown();
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(&profile);
}
