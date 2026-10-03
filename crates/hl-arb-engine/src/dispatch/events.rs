//! Inbound event handling: market/timer dispatch, account updates, and fills.

use hl_arb_client::RejectReason;
use hl_arb_strategy::StrategyId;
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::exec::apply_post_ack;
use crate::journal::JournalEntry;
use crate::orders::OrderState;
use crate::risk::cancel_all_cloids;
use crate::state::EngineState;
use crate::strategy::{Ctx, OrderEvent, OrderEventKind};
use crate::timers::TimerId as HeapTimerId;
use crate::types::{AccountUpdate, Cloid, CoinId, Control, PostResult, Px, Side, Stamp, Sz};

use super::*;

impl StrategyDispatcher {
    pub(super) fn dispatch_coin(&mut self, coin: CoinId, stamp: Stamp, state: &EngineState) {
        // Apply any paper updates queued by a prior iteration (notably the
        // kill-switch cancels) even while halted, so `simulate` reflects the
        // cancellation within one iteration.
        self.tick_paper(state);
        if self.halted {
            return;
        }
        let indices: SmallVec<[usize; 8]> = self.routes.for_coin(coin).iter().copied().collect();
        if indices.is_empty() {
            return;
        }
        // Event time of the update that marked this coin dirty (SPEC-0010 §8):
        // fall back to the slot's freshest stamp if the caller passed none.
        let t_recv = if stamp.mono_ns > 0 {
            stamp.mono_ns
        } else {
            slot_mono(state.slot(coin)).unwrap_or(0)
        };
        for index in indices {
            if self.paused.get(index).copied().unwrap_or(false) {
                continue;
            }
            if !wants_coin(&self.strategy_interests[index], coin) {
                continue;
            }
            let ctx = Ctx {
                now: stamp,
                markets: state.slots(),
                account: &self.account,
                registry: &self.registry,
            };
            self.strategies[index].on_market(coin, &ctx, &mut self.actions);
        }
        let t_decided = self.clock.mono_ns();
        self.run_actions(stamp, state, t_recv, t_decided);
        self.tick_paper(state);
    }

    pub(super) fn dispatch_timer(&mut self, id: HeapTimerId, stamp: Stamp, state: &EngineState) {
        self.tick_paper(state);
        if self.halted {
            return;
        }
        let Some(&(index, timer)) = self.timer_owners.get(&id) else {
            return;
        };
        if self.paused.get(index).copied().unwrap_or(false) {
            return;
        }
        let ctx = Ctx {
            now: stamp,
            markets: state.slots(),
            account: &self.account,
            registry: &self.registry,
        };
        self.strategies[index].on_timer(timer, &ctx, &mut self.actions);
        let t_decided = self.clock.mono_ns();
        self.run_actions(stamp, state, 0, t_decided);
        self.tick_paper(state);
    }

    pub(super) fn apply_account(&mut self, update: &AccountUpdate, state: &EngineState) {
        self.stream.on_account_update(update);
        let stamp = account_stamp(update);
        let t_recv = stamp.mono_ns;

        match update {
            AccountUpdate::Control(control) => self.handle_control(control),
            AccountUpdate::OrderUpdate {
                cloid,
                oid,
                status,
                filled_sz,
                avg_px,
                ..
            } => {
                self.orders
                    .on_order_update(*cloid, *status, *filled_sz, *avg_px);
                self.orders.record_oid(*oid, *cloid);
                let event = self.orders.get(*cloid).map(|order| {
                    (
                        order.strategy.clone(),
                        OrderEvent {
                            stamp,
                            cloid: Some(*cloid),
                            oid: *oid,
                            coin: order.coin,
                            side: strat_side(order.side),
                            px: *avg_px,
                            sz: Decimal::ZERO,
                            fee: Decimal::ZERO,
                            maker: false,
                            reduce_only: order.reduce_only,
                            kind: OrderEventKind::Status(*status),
                        },
                    )
                });
                if let Some((owner, event)) = event {
                    self.deliver_to(&owner, &event, state);
                }
            }
            AccountUpdate::Fill {
                cloid,
                oid,
                tid,
                coin,
                side,
                px,
                sz,
                fee,
                ..
            } => {
                // Live fills skip tids already seen (a reconnect snapshot may
                // re-deliver them).
                if self.fills.observe_live(*tid) {
                    self.apply_fill(*cloid, *oid, *coin, *side, *px, *sz, *fee, stamp, state);
                }
            }
            AccountUpdate::Fills { fills, stamp } => {
                if self.fills.first_snapshot {
                    // A reconnect snapshot applies only the tids missed during
                    // the gap (the resync H-3 asks for).
                    for fill in fills {
                        if self.fills.observe(fill.tid) {
                            self.apply_fill(
                                fill.cloid, fill.oid, fill.coin, fill.side, fill.px, fill.sz,
                                fill.fee, *stamp, state,
                            );
                        }
                    }
                } else {
                    // The first-connect snapshot is already in the starting
                    // position: record its tids without applying them.
                    for fill in fills {
                        self.fills.record(fill.tid);
                    }
                    self.fills.first_snapshot = true;
                }
            }
            AccountUpdate::ResolveUnknown { cloid, status, .. } => {
                // Apply the venue's answer only while the order is still
                // `Unknown`: a stale answer after a newer stream update must not
                // move the order backwards (SPEC-0002 H-2).
                if self
                    .orders
                    .get(*cloid)
                    .is_some_and(|order| order.state.is_unknown())
                {
                    let _ = crate::reconcile::resolve_unknown(&mut self.orders, *cloid, status);
                }
            }
            AccountUpdate::UnknownExpired { cloid, .. } => {
                // The bounded `orderStatus` retries never resolved the order and
                // it can no longer land: resolve it as not placed.
                if self
                    .orders
                    .get(*cloid)
                    .is_some_and(|order| order.state.is_unknown())
                {
                    self.orders
                        .set_state(*cloid, OrderState::Rejected(RejectReason::Unknown));
                }
            }
            AccountUpdate::PostAck { req_id, result, .. } => {
                self.handle_post_ack(*req_id, result, stamp, state);
            }
            AccountUpdate::Reconcile { snapshot, .. } => {
                self.account.account_value = snapshot.account_value;
                self.account.margin_used = snapshot.margin_used;
            }
            AccountUpdate::Funding { usdc, .. } => {
                // Funding is realized PnL; it moves account value, not position.
                // If a future venue reports otherwise, defer to the reconcile.
                self.account.account_value += *usdc;
            }
        }

        // SPEC-0010 §16: resume new places once every `Unknown` order has been
        // reconciled. Only the self-resolving `exec_error` breaker is cleared
        // here; `exec_backpressure` and operator breakers stay tripped.
        if !self.orders.has_unknown() {
            self.risk.breakers_mut().clear_label("exec_error");
        }

        let t_decided = self.clock.mono_ns();
        self.run_actions(stamp, state, t_recv, t_decided);
    }

    /// Apply one already-deduplicated fill: move the position, advance the
    /// order's filled size, and deliver a fill event to the owning strategy.
    ///
    /// A fill is mapped to its order by `cloid` (when the wire carried one),
    /// else by the `oid → cloid` index, else by the recent-route cache for an
    /// order pruned after it filled. A fill that arrives after the order's
    /// terminal update and after pruning still reaches the strategy.
    #[allow(clippy::too_many_arguments)]
    fn apply_fill(
        &mut self,
        cloid: Option<Cloid>,
        oid: u64,
        coin: CoinId,
        side: Side,
        px: Px,
        sz: Sz,
        fee: Px,
        stamp: Stamp,
        state: &EngineState,
    ) {
        let cloid = cloid
            .or_else(|| self.orders.cloid_for_oid(oid))
            .or_else(|| self.recent_routes.get(&oid).map(|(cloid, _)| *cloid));

        if self.journal.is_some() {
            let entry = JournalEntry::Fill {
                cloid: cloid.map(|cloid| cloid.to_hex()),
                coin: self.registry.coin(coin).unwrap_or("").to_string(),
                side: venue_side_str(side).to_string(),
                px,
                sz,
                fee,
            };
            if let Some(sink) = self.journal.as_mut() {
                sink.record(&entry);
            }
        }

        // A perp fill moves the position directly; a spot fill is left to the
        // reconciler (balances are keyed by token, not coin).
        if !self.is_spot(coin) {
            let signed = if matches!(side, Side::Buy) { sz } else { -sz };
            let current = self.account.position_szi(coin);
            self.account.set_position_szi(coin, current + signed);
        }
        if let Some(cloid) = cloid {
            self.orders.on_fill(Some(cloid), sz);
        }

        let route = cloid
            .and_then(|cloid| {
                self.orders
                    .get(cloid)
                    .map(|order| (cloid, order.strategy.clone(), order.reduce_only))
            })
            .or_else(|| {
                self.recent_routes
                    .get(&oid)
                    .map(|(cloid, owner)| (*cloid, owner.clone(), false))
            });
        if let Some((cloid, owner, reduce_only)) = route {
            let event = OrderEvent {
                stamp,
                cloid: Some(cloid),
                oid,
                coin,
                side: strat_side(side),
                px,
                sz,
                fee,
                maker: false,
                reduce_only,
                kind: OrderEventKind::Fill,
            };
            self.deliver_to(&owner, &event, state);
        }
    }

    fn handle_control(&mut self, control: &Control) {
        match control {
            Control::KillSwitch => {
                let _ = self.risk.kill().set();
                self.halted = true;
                for cloid in cancel_all_cloids(&self.orders) {
                    self.actions.cancel(cloid);
                }
            }
            Control::Resume => {
                let _ = self.risk.kill().clear();
                self.halted = false;
            }
            Control::Pause { strategy } => {
                let id = StrategyId::from(strategy.clone());
                if let Some(&index) = self.strategy_by_id.get(&id) {
                    self.paused[index] = true;
                }
            }
            Control::ReloadLimits => {
                // Re-reading config and mutating the gate is the caller's job.
            }
        }
    }

    fn handle_post_ack(
        &mut self,
        req_id: u64,
        result: &PostResult,
        stamp: Stamp,
        state: &EngineState,
    ) {
        match result {
            PostResult::Statuses(acks) => {
                let cloids = self.req_cloids.remove(&req_id);
                apply_post_ack(&mut self.orders, req_id, result);
                let Some(cloids) = cloids else {
                    return;
                };
                for (cloid, ack) in cloids.iter().zip(acks.iter()) {
                    let event = self.orders.get(*cloid).map(|order| {
                        (
                            order.strategy.clone(),
                            OrderEvent {
                                stamp,
                                cloid: Some(*cloid),
                                oid: ack.oid.unwrap_or(0),
                                coin: order.coin,
                                side: strat_side(order.side),
                                px: Decimal::ZERO,
                                sz: Decimal::ZERO,
                                fee: Decimal::ZERO,
                                maker: false,
                                reduce_only: order.reduce_only,
                                kind: OrderEventKind::Status(ack.status),
                            },
                        )
                    });
                    if let Some((owner, event)) = event {
                        self.deliver_to(&owner, &event, state);
                    }
                }
            }
            PostResult::Rejected(reason) => {
                // The venue said no (or the post was never sent): the orders are
                // terminal `Rejected`, not `Unknown`, so the breaker clears.
                tracing::debug!(
                    req_id,
                    %reason,
                    "post rejected before/at the venue; orders terminal"
                );
                self.reject_requests(&[req_id]);
            }
            PostResult::Error(reason) => {
                // A lost reply does not mean the order was rejected: it may be
                // resting. Mark it `Unknown` (fail closed), trip the breaker to
                // halt new places, and let the exec layer reconcile it by cloid
                // via `orderStatus` (SPEC-0010 §10/§16, SPEC-0002 H-2). An
                // unknown order still counts for exposure and the dead-man
                // switch, and blocks new non-reduce-only places on its coin.
                self.risk.breakers_mut().trip("exec_error");
                tracing::warn!(
                    req_id,
                    %reason,
                    "post ack error: orders marked Unknown, breaker tripped"
                );
                let Some(cloids) = self.req_cloids.remove(&req_id) else {
                    return;
                };
                for cloid in &cloids {
                    self.orders.set_state(*cloid, OrderState::Unknown);
                }
            }
        }
    }

    fn deliver_to(&mut self, owner: &StrategyId, event: &OrderEvent, state: &EngineState) {
        if self.halted {
            return;
        }
        let Some(&index) = self.strategy_by_id.get(owner) else {
            return;
        };
        if self.paused.get(index).copied().unwrap_or(false) {
            return;
        }
        let ctx = Ctx {
            now: event.stamp,
            markets: state.slots(),
            account: &self.account,
            registry: &self.registry,
        };
        self.strategies[index].on_order(event, &ctx, &mut self.actions);
    }

    fn is_spot(&self, coin: CoinId) -> bool {
        self.table.meta(coin).is_some_and(|meta| meta.is_spot)
    }
}
