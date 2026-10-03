//! Dead-man's switch via `scheduleCancel` (SPEC-0002 §12).
//!
//! In `live`, arming `scheduleCancel` tells the venue to cancel all resting
//! orders at `time`. We refresh it on a heartbeat so that if the process stalls
//! or loses connectivity, the venue cancels stale orders after the TTL. A
//! graceful shutdown disarms it explicitly. This module owns the pure decision
//! logic (when to arm/refresh/disarm); the executor submits the actions and
//! tracks arm state as a metric/health input.

use crate::order::Action;

/// Default dead-man TTL: 120 s (SPEC-0002 H-4). The switch is refreshed at half
/// the TTL, which keeps ~5.8k arm/refresh requests/day (well inside budget)
/// for a TTL long enough to tolerate a stall.
pub const DEFAULT_TTL_MS: u64 = 120_000;

/// Default refresh margin: refresh once less than half the TTL remains.
pub const DEFAULT_REFRESH_MARGIN_MS: u64 = 15_000;

/// Tracks the desired `scheduleCancel` armed time.
#[derive(Debug, Clone)]
pub struct DeadMansSwitch {
    ttl_ms: u64,
    refresh_margin_ms: u64,
    armed_until: Option<u64>,
    arms: u64,
}

impl DeadMansSwitch {
    /// Create a switch with the given TTL and the default refresh margin.
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            refresh_margin_ms: ttl_ms / 2,
            armed_until: None,
            arms: 0,
        }
    }

    /// Override the refresh margin (clamped to at most the TTL).
    pub fn with_refresh_margin(mut self, margin_ms: u64) -> Self {
        self.refresh_margin_ms = margin_ms.min(self.ttl_ms);
        self
    }

    /// Whether the switch is currently armed.
    pub fn is_armed(&self) -> bool {
        self.armed_until.is_some()
    }

    /// The currently armed cancel-at time, if any.
    pub fn armed_until(&self) -> Option<u64> {
        self.armed_until
    }

    /// Number of arm/refresh actions emitted (for metrics).
    pub fn arms(&self) -> u64 {
        self.arms
    }

    /// Arm (or re-arm) and return the `scheduleCancel` action to submit.
    pub fn arm(&mut self, now_ms: u64) -> Action {
        let at = now_ms.saturating_add(self.ttl_ms);
        self.armed_until = Some(at);
        self.arms += 1;
        Action::ScheduleCancel { time: Some(at) }
    }

    /// Return a refresh action when the current arm is close to expiring.
    ///
    /// Refreshes when the remaining time falls to or below the refresh margin,
    /// so a healthy heartbeat keeps the venue armed without spamming.
    pub fn refresh_if_due(&mut self, now_ms: u64) -> Option<Action> {
        let at = self.armed_until?;
        let remaining = at.saturating_sub(now_ms);
        if remaining <= self.refresh_margin_ms {
            Some(self.arm(now_ms))
        } else {
            None
        }
    }

    /// Disarm and return the `scheduleCancel` action to submit (or `None` when
    /// it was never armed).
    pub fn disarm(&mut self) -> Option<Action> {
        if self.armed_until.take().is_some() {
            Some(Action::ScheduleCancel { time: None })
        } else {
            None
        }
    }

    /// Reconcile the switch with the number of resting orders and return the
    /// action to submit, if any (SPEC-0002 H-4).
    ///
    /// The switch is armed **only** while at least one order rests, so it does
    /// not spend address rate-limit budget when there is nothing to protect,
    /// and disarmed as soon as the last order leaves. A missing disarm attempt
    /// is safe: the venue expires the schedule on its own.
    pub fn update(&mut self, now_ms: u64, resting_orders: usize) -> Option<Action> {
        if resting_orders == 0 {
            return self.disarm();
        }
        if !self.is_armed() {
            return Some(self.arm(now_ms));
        }
        self.refresh_if_due(now_ms)
    }

    /// Whether a failure to arm/refresh must halt trading (SPEC-0002 H-4):
    /// while orders are resting, an unarmed or failed switch is unsafe.
    pub fn requires_arm(&self, resting_orders: usize) -> bool {
        resting_orders > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_sets_deadline_and_action() {
        let mut switch = DeadMansSwitch::new(30_000);
        assert!(!switch.is_armed());
        let action = switch.arm(1_000_000);
        assert_eq!(switch.armed_until(), Some(1_030_000));
        assert!(switch.is_armed());
        assert_eq!(
            action,
            Action::ScheduleCancel {
                time: Some(1_030_000)
            }
        );
    }

    #[test]
    fn refresh_only_near_expiry() {
        let mut switch = DeadMansSwitch::new(30_000);
        switch.arm(1_000_000); // expires 1_030_000, margin 15_000
        assert!(switch.refresh_if_due(1_010_000).is_none()); // 20s left
        let refreshed = switch.refresh_if_due(1_020_000).expect("due"); // 10s left
        assert_eq!(switch.armed_until(), Some(1_050_000));
        assert_eq!(
            refreshed,
            Action::ScheduleCancel {
                time: Some(1_050_000)
            }
        );
        assert_eq!(switch.arms(), 2);
    }

    #[test]
    fn disarm_emits_none_only_once() {
        let mut switch = DeadMansSwitch::new(30_000);
        switch.arm(0);
        assert_eq!(switch.disarm(), Some(Action::ScheduleCancel { time: None }));
        assert!(!switch.is_armed());
        assert_eq!(switch.disarm(), None);
    }

    #[test]
    fn update_arms_only_while_orders_rest() {
        let mut switch = DeadMansSwitch::new(120_000);
        let t0 = 1_700_000_000_000u64;
        // No resting orders: nothing to arm.
        assert_eq!(switch.update(t0, 0), None);
        assert!(!switch.is_armed());
        // First resting order arms.
        assert_eq!(
            switch.update(t0, 1),
            Some(Action::ScheduleCancel {
                time: Some(t0 + 120_000)
            })
        );
        assert!(switch.is_armed());
        // Still resting, not yet due (60s left, not <= 60s margin is false;
        // use 61s left): no action.
        assert_eq!(switch.update(t0 + 59_000, 2), None);
        // Last order gone: disarm.
        assert_eq!(
            switch.update(t0 + 60_000, 0),
            Some(Action::ScheduleCancel { time: None })
        );
        assert!(!switch.is_armed());
    }

    #[test]
    fn update_refreshes_near_expiry_while_resting() {
        let mut switch = DeadMansSwitch::new(120_000);
        let t0 = 1_700_000_000_000u64;
        switch.update(t0, 1); // expires t0+120_000, margin 60_000
        assert_eq!(switch.update(t0 + 30_000, 1), None); // 90s left
        let refreshed = switch.update(t0 + 70_000, 1).expect("due"); // 50s left
        assert_eq!(
            refreshed,
            Action::ScheduleCancel {
                time: Some(t0 + 190_000)
            }
        );
    }

    #[test]
    fn default_ttl_is_two_minutes() {
        assert_eq!(DEFAULT_TTL_MS, 120_000);
    }
}
