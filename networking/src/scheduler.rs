//! Resource scheduling: priority-ordered request dispatch.
//!
//! Pages load faster when scripts/styles/images are fetched in the order
//! the renderer needs them. The scheduler is a small priority queue the
//! engine consults before issuing parallel fetches, with per-origin
//! concurrency caps matching browser behavior (6 per origin for h1,
//! unlimited multiplexing for h2/h3).

use std::collections::BinaryHeap;

/// Resource priority (higher = more urgent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourcePriority {
    /// Navigation/main frame (highest).
    Navigation = 100,
    /// Blocking stylesheets.
    CriticalStyle = 90,
    /// Blocking scripts.
    CriticalScript = 80,
    /// Fonts (needed for first paint of text).
    Font = 60,
    /// Async scripts, XHR.
    AsyncScript = 50,
    /// Images above the fold.
    HighImage = 40,
    /// Everything else.
    Normal = 30,
    /// Below-the-fold / speculative resources.
    Low = 10,
}

/// A queued fetch operation.
#[derive(Debug)]
pub struct QueuedFetch {
    /// Priority.
    pub priority: ResourcePriority,
    /// Insertion sequence (FIFO within a priority).
    pub sequence: u64,
}

impl Eq for QueuedFetch {}

impl PartialEq for QueuedFetch {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl PartialOrd for QueuedFetch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedFetch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap: higher priority pops first; within one
        // priority, earlier sequence pops first (FIFO).
        self.priority
            .cmp(&other.priority)
            .then(other.sequence.cmp(&self.sequence))
    }
}

/// Priority queue with per-origin concurrency tracking.
#[derive(Debug, Default)]
pub struct ResourceScheduler {
    queue: BinaryHeap<QueuedFetch>,
    sequence: u64,
    in_flight: std::collections::HashMap<String, u32>,
    max_per_origin: u32,
}

impl ResourceScheduler {
    /// Creates the scheduler with default browser-like limits.
    pub fn new() -> Self {
        ResourceScheduler {
            queue: BinaryHeap::new(),
            sequence: 0,
            in_flight: std::collections::HashMap::new(),
            max_per_origin: 6,
        }
    }

    /// Enqueues a fetch; returns its sequence number.
    pub fn enqueue(&mut self, priority: ResourcePriority) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        self.queue.push(QueuedFetch { priority, sequence });
        sequence
    }

    /// Pops the next fetch allowed under the per-origin limit.
    pub fn next(&mut self, origin: &str) -> Option<u64> {
        let in_flight = self.in_flight.get(origin).copied().unwrap_or(0);
        if in_flight >= self.max_per_origin {
            return None;
        }
        self.queue.pop().map(|fetch| {
            *self.in_flight.entry(origin.to_owned()).or_insert(0) += 1;
            fetch.sequence
        })
    }

    /// Records a completed fetch.
    pub fn complete(&mut self, origin: &str) {
        if let Some(count) = self.in_flight.get_mut(origin) {
            *count = count.saturating_sub(1);
        }
    }

    /// Pending item count.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Total in-flight requests.
    pub fn in_flight(&self) -> u32 {
        self.in_flight.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ordering() {
        let mut scheduler = ResourceScheduler::new();
        let low = scheduler.enqueue(ResourcePriority::Low);
        let critical = scheduler.enqueue(ResourcePriority::CriticalStyle);
        let normal = scheduler.enqueue(ResourcePriority::Normal);
        let first = scheduler.next("https://example.com").unwrap();
        let second = scheduler.next("https://example.com").unwrap();
        let third = scheduler.next("https://example.com").unwrap();
        assert_eq!(first, critical);
        assert_eq!(second, normal);
        assert_eq!(third, low);
    }

    #[test]
    fn per_origin_limit() {
        let mut scheduler = ResourceScheduler::new();
        for _ in 0..10 {
            scheduler.enqueue(ResourcePriority::Normal);
        }
        let mut granted = 0;
        while scheduler.next("https://example.com").is_some() {
            granted += 1;
        }
        assert_eq!(granted, 6);
        // Other origins are unaffected.
        assert!(scheduler.next("https://other.example").is_some());
        // Completing frees a slot.
        scheduler.complete("https://example.com");
        assert!(scheduler.next("https://example.com").is_some());
    }
}
