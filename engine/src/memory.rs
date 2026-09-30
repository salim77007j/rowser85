//! Memory manager: budget enforcement, GC scheduling, frame eviction.
//!
//! Goals: idle CPU ≈ 0, no unbounded growth, graceful degradation under
//! memory pressure instead of OOM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{EngineEvent, EngineLoop};

/// Static state for the current memory tick (shared with page threads
/// through their JsRuntime handles indirectly).
static PRESSURE: AtomicBool = AtomicBool::new(false);

/// One memory-management pass.
pub fn tick(state: &EngineLoop) {
    let total = state.total_tab_memory();
    let total_ram = total_system_memory();
    let budget = (total_ram as f64 * state.config.memory_budget_fraction as f64) as u64;

    if total > budget {
        // Under pressure: ask suspended/backgrounded tabs to shed frames,
        // request GC on all tabs.
        tracing::warn!(
            target: "rowser::memory",
            "pressure: {} KiB used of {} KiB budget",
            total / 1024,
            budget / 1024
        );
        PRESSURE.store(true, Ordering::Relaxed);
        let _ = state.event_tx.send(EngineEvent::MemoryPressure { total_bytes: total });
        // Suspend the largest backgrounded tab immediately.
        let mut tabs = state.pages().lock().unwrap();
        let mut largest: Option<(u64, crate::TabId)> = None;
        for (tab, handle) in tabs.iter() {
            if !handle.focused && !handle.suspended {
                if largest.map(|(mem, _)| handle.memory > mem).unwrap_or(true) {
                    largest = Some((handle.memory, *tab));
                }
            }
        }
        if let Some((_, tab)) = largest {
            if let Some(handle) = tabs.get_mut(&tab) {
                handle.suspended = true;
                let _ = handle.tx.send(crate::page::Message::Suspend);
                let _ = state.event_tx.send(EngineEvent::TabSuspended(tab));
            }
        }
    } else {
        PRESSURE.store(false, Ordering::Relaxed);
    }
}

/// True when the engine is under memory pressure.
pub fn under_pressure() -> bool {
    PRESSURE.load(Ordering::Relaxed)
}

/// Total system RAM in bytes (sysinfo, cached).
pub fn total_system_memory() -> u64 {
    use std::sync::OnceLock;
    static TOTAL: OnceLock<u64> = OnceLock::new();
    *TOTAL.get_or_init(|| {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        system.total_memory()
    })
}

/// Recommended page-cache budget scaling with RAM.
pub fn page_cache_budget() -> u64 {
    (total_system_memory() / 8).clamp(32 * 1024 * 1024, 2 * 1024 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    #[test]
    fn total_memory_sane() {
        let total = super::total_system_memory();
        assert!(total > 256 * 1024 * 1024, "system memory: {total}");
    }
}
