//! A monotonic timer heap (SPEC-0010 §7, §9).
//!
//! Timers are addressed by a stable [`TimerId`] and fire when the engine's
//! monotonic clock passes their deadline. The heap is a `BinaryHeap` of
//! `(deadline, id)`; ties break on the id so firing order is deterministic.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// A caller-chosen timer identifier, unique per engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerId(pub u64);

/// A single scheduled timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    deadline_ns: u64,
    id: TimerId,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Ascending by deadline, ties by id; wrapped in `Reverse` below so the
        // `BinaryHeap` (a max-heap) pops the smallest entry first.
        self.deadline_ns
            .cmp(&other.deadline_ns)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A min-heap of timers keyed by deadline.
#[derive(Debug, Default)]
pub struct TimerHeap {
    heap: BinaryHeap<Reverse<Entry>>,
}

impl TimerHeap {
    /// An empty heap.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending timers.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether no timers are pending.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Schedule `id` to fire at `deadline_ns`.
    ///
    /// A re-arm of an existing `id` is allowed: both entries fire, and the
    /// caller is expected to cancel or ignore the stale one (strategies own
    /// their timer ids).
    pub fn schedule(&mut self, id: TimerId, deadline_ns: u64) {
        self.heap.push(Reverse(Entry { deadline_ns, id }));
    }

    /// The earliest deadline, if any.
    pub fn next_deadline(&self) -> Option<u64> {
        self.heap.peek().map(|entry| entry.0.deadline_ns)
    }

    /// Pop every timer whose deadline is `<= now_ns`, in firing order.
    pub fn pop_due(&mut self, now_ns: u64) -> Vec<TimerId> {
        let mut due = Vec::new();
        while let Some(entry) = self.heap.peek() {
            if entry.0.deadline_ns > now_ns {
                break;
            }
            due.push(self.heap.pop().expect("peeked").0.id);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_in_deadline_order_then_id_order() {
        let mut heap = TimerHeap::new();
        heap.schedule(TimerId(2), 100);
        heap.schedule(TimerId(0), 50);
        heap.schedule(TimerId(1), 100); // same deadline as id 2
        // Nothing due before 50.
        assert_eq!(heap.pop_due(49), Vec::<TimerId>::new());
        // At 100 all fire: id 0 (50), then the 100s by id.
        assert_eq!(heap.pop_due(100), vec![TimerId(0), TimerId(1), TimerId(2)]);
        assert!(heap.is_empty());
    }

    #[test]
    fn next_deadline_tracks_the_minimum() {
        let mut heap = TimerHeap::new();
        assert_eq!(heap.next_deadline(), None);
        heap.schedule(TimerId(0), 500);
        heap.schedule(TimerId(1), 200);
        assert_eq!(heap.next_deadline(), Some(200));
        heap.pop_due(200);
        assert_eq!(heap.next_deadline(), Some(500));
    }

    #[test]
    fn rearming_the_same_id_fires_both() {
        let mut heap = TimerHeap::new();
        heap.schedule(TimerId(7), 10);
        heap.schedule(TimerId(7), 20);
        assert_eq!(heap.pop_due(20), vec![TimerId(7), TimerId(7)]);
    }
}
