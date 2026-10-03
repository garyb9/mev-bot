//! The action pipeline: risk gating, cloid assignment, batch planning, and send.

use hl_arb_client::RejectReason;
use hl_arb_risk::Decision;
use hl_arb_strategy::OrderIntent;
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::builder::plan_iteration;
use crate::exec::dispatch;
use crate::instrument::Stamps;
use crate::journal::JournalEntry;
use crate::orders::{LiveOrder, OrderState};
use crate::paper_exec::{paper_cancels_from_post, paper_orders_from_post};
use crate::risk::RiskCtx;
use crate::state::{EngineState, MarketSlot};
use crate::strategy::Action;
use crate::types::{AccountUpdate, Cloid, CoinId, Px, Side, Stamp};

use super::*;

impl StrategyDispatcher {
    /// Gate the buffered actions, assign cloids, insert live orders, plan the
    /// batch, and dispatch it (SPEC-0010 §10–§12).
    pub(super) fn run_actions(
        &mut self,
        stamp: Stamp,
        state: &EngineState,
        t_recv: u64,
        t_decided: u64,
    ) {
        if self.actions.is_empty() {
            return;
        }
        let actions = self.actions.take();
        self.actions.clear();

        let now_ms = self.clock.now_ms();
        let mut approved: SmallVec<[Action; 16]> = SmallVec::new();
        for mut action in actions {
            if self.gate_action(&mut action, state, now_ms) {
                approved.push(action);
            }
        }

        let t_risked = self.clock.mono_ns();
        if approved.is_empty() {
            return;
        }
        self.record_journal(&approved);
        self.plan_and_dispatch(&approved, state, t_recv, stamp.mono_ns, t_decided, t_risked);
    }

    /// Append the risk-approved actions to the journal, if one is attached
    /// (SPEC-0010 §14). Entries are built before the sink borrow to keep the
    /// order-manager lookup separate.
    fn record_journal(&mut self, approved: &[Action]) {
        if self.journal.is_none() {
            return;
        }
        let mut entries: SmallVec<[JournalEntry; 16]> = SmallVec::new();
        for action in approved {
            match action {
                Action::Place(intent) => entries.push(JournalEntry::Place {
                    cloid: intent.cloid.clone().unwrap_or_default(),
                    coin: intent.coin.clone(),
                    side: strat_side_str(intent.side).to_string(),
                    limit_px: intent.limit_px,
                    size: intent.size,
                }),
                Action::Cancel { cloid } => entries.push(JournalEntry::Cancel {
                    cloid: cloid.to_hex(),
                }),
                Action::Modify { cloid, px, sz } => entries.push(JournalEntry::Modify {
                    cloid: cloid.to_hex(),
                    px: *px,
                    sz: *sz,
                }),
                Action::PlaceGroup(_) => {}
            }
        }
        if let Some(sink) = self.journal.as_mut() {
            for entry in &entries {
                sink.record(entry);
            }
        }
    }

    /// Gate one action. Returns whether it should proceed to the builder.
    ///
    /// The check is done before any mutable borrow of `action`, so a resized
    /// place can then be rewritten in place.
    fn gate_action(&mut self, action: &mut Action, state: &EngineState, now_ms: u64) -> bool {
        // The stream halt only blocks risk-increasing places.
        if let Action::Place(intent) = action
            && self.stream.places_halted()
            && !intent.reduce_only
        {
            return false;
        }

        // Resolve the coin without a live mutable borrow of `action`.
        let coin = match action {
            Action::Place(intent) => self.registry.id(&intent.coin),
            Action::Cancel { cloid } => self.orders.get(*cloid).map(|order| order.coin),
            Action::Modify { cloid, .. } => self.orders.get(*cloid).map(|order| order.coin),
            Action::PlaceGroup(_) => return false,
        };
        let Some(coin) = coin else {
            // Unknown coin/cloid: cancels/modifies reduce risk and are left for
            // the builder to drop; an unknown place is dropped here.
            return !matches!(action, Action::Place(_));
        };
        let (Some(meta), Some(slot)) = (self.table.meta(coin), state.slot(coin)) else {
            return !matches!(action, Action::Place(_));
        };

        let decision = {
            let ctx = RiskCtx {
                coin,
                orders: &self.orders,
                account: &self.account,
                slot,
                meta: &meta,
                rate: &self.rate,
                now_ms,
            };
            self.risk.check(action, &ctx)
        };

        match (action, decision) {
            (Action::Place(_), Decision::Reject(_)) => false,
            (Action::Place(intent), Decision::Approve) => {
                self.finish_place(coin, intent, slot);
                true
            }
            (Action::Place(intent), Decision::Resize(size)) => {
                intent.size = size;
                self.finish_place(coin, intent, slot);
                true
            }
            (Action::Cancel { .. } | Action::Modify { .. }, decision) => decision.is_allowed(),
            (Action::PlaceGroup(_), _) => false,
        }
    }

    /// Assign a cloid, record the owner, and insert a `PendingNew` live order.
    fn finish_place(&mut self, coin: CoinId, intent: &mut OrderIntent, slot: &MarketSlot) {
        let cloid = intent
            .cloid
            .as_deref()
            .and_then(Cloid::from_hex)
            .unwrap_or_else(|| self.assigner.next());
        intent.cloid = Some(cloid.to_hex());
        let px = intent
            .limit_px
            .or_else(|| mid(slot))
            .unwrap_or(Decimal::ZERO);
        let order = LiveOrder {
            cloid,
            coin,
            side: if intent.side.is_buy() {
                Side::Buy
            } else {
                Side::Sell
            },
            px,
            sz: intent.size,
            filled_sz: Decimal::ZERO,
            reduce_only: intent.reduce_only,
            strategy: intent.strategy.clone(),
            state: OrderState::PendingNew,
            req_id: None,
            oid: None,
        };
        self.owners.insert(cloid, intent.strategy.clone());
        self.orders.insert(order);
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_and_dispatch(
        &mut self,
        approved: &[Action],
        state: &EngineState,
        t_recv: u64,
        t_dequeued: u64,
        t_decided: u64,
        t_risked: u64,
    ) {
        let touch = |coin: CoinId, is_buy: bool| -> Option<Px> {
            let slot = state.slot(coin)?;
            // An aggressive order only needs the side it crosses: ask for a
            // buy, bid for a sell. The other side being empty is not a reason
            // to drop it.
            let level = if is_buy {
                slot.best_ask()
            } else {
                slot.best_bid()
            }?;
            Some(level.px)
        };
        let mut batch = plan_iteration(
            approved,
            &self.registry,
            &self.table,
            &self.orders,
            &self.assigner,
            &touch,
            self.config.max_slippage_bps,
            &mut self.req_ids,
        );
        // Carry the market frame's monotonic read time so the live transport
        // can stamp the end-to-end `hl_tick_to_order_seconds` after the socket
        // write (SPEC-0002 H-7).
        for post in &mut batch.posts {
            post.recv_mono_ns = t_recv;
        }

        // A built batch may drop places (bad size, min notional, missing asset).
        // Remove their provisional live orders so exposure is not stranded.
        for (cloid, _reason) in &batch.dropped {
            self.orders.remove(*cloid);
            self.owners.remove(cloid);
        }

        let batch_reqs: SmallVec<[u64; 2]> = batch.posts.iter().map(|post| post.req_id).collect();
        let paper_mode = self.exec.is_none() && self.paper.is_some();
        if !paper_mode {
            for post in &batch.posts {
                self.req_cloids.insert(post.req_id, post.cloids.clone());
            }
        }

        let t_signed = self.clock.mono_ns();
        if let Some(exec) = self.exec.as_mut() {
            if dispatch(batch, exec, &mut self.orders).is_err() {
                // Outbound channel full or exec down: fail closed. Rare, so
                // a WARN is allowed (never per-event at INFO; §4).
                let _ = self.risk.breakers_mut().trip("exec_backpressure");
                tracing::warn!(
                    drop = batch_reqs.len(),
                    "exec backpressure: batch rejected, breaker tripped"
                );
                self.reject_requests(&batch_reqs);
            }
        } else if paper_mode {
            // `simulate`: the paper backend fills synchronously; its account
            // updates are applied on this iteration's paper tick.
            let updates = self.feed_paper(&batch, self.clock.now_ms());
            self.paper_updates.extend(updates);
        } else {
            // No backend (`observe`): never leave orders pending. Fail closed.
            self.reject_requests(&batch_reqs);
        }
        let t_handoff = self.clock.mono_ns();

        // `t_written`/`t_ack` come from the exec layer and are unset here, so the
        // headline tick-to-order and handoff histograms are not recorded until
        // the exec writer reports; the engine-side decide/risk/sign spans are.
        self.recorder.record(&Stamps {
            t_recv,
            t_decoded: 0,
            t_dequeued,
            t_decided,
            t_risked,
            t_signed,
            t_handoff,
            t_written: 0,
            t_ack: 0,
        });
    }

    /// Feed one built batch to the paper backend, returning its immediate
    /// account updates (cancel acknowledgements; places are queued for latency).
    fn feed_paper(
        &mut self,
        batch: &crate::builder::BuiltBatch,
        now_ms: u64,
    ) -> Vec<AccountUpdate> {
        let Some(paper) = self.paper.as_mut() else {
            return Vec::new();
        };
        let mut updates = Vec::new();
        for post in &batch.posts {
            match &post.action {
                hl_arb_client::Action::Order { .. } => {
                    let orders = paper_orders_from_post(post, &self.registry, &self.table);
                    updates.extend(paper.submit(&orders, &self.registry, &[], now_ms));
                }
                hl_arb_client::Action::CancelByCloid { .. } => {
                    let cancels = paper_cancels_from_post(post);
                    updates.extend(paper.cancel(&cancels, now_ms));
                }
                _ => {}
            }
        }
        updates
    }

    /// Advance the paper backend against the current books and fold its account
    /// updates (fills, status changes) back through [`Self::apply_account`].
    pub(super) fn tick_paper(&mut self, state: &EngineState) {
        if self.paper.is_none() {
            return;
        }
        let now_ms = self.clock.now_ms();
        let mut updates = std::mem::take(&mut self.paper_updates);
        if let Some(paper) = self.paper.as_mut() {
            updates.extend(paper.on_market(&self.registry, state.slots(), now_ms));
        }
        for update in &updates {
            self.apply_account(update, state);
        }
    }

    /// Mark every `PendingNew` order from these requests rejected (fail closed).
    pub(super) fn reject_requests(&mut self, reqs: &[u64]) {
        for req_id in reqs {
            let Some(cloids) = self.req_cloids.remove(req_id) else {
                continue;
            };
            for cloid in cloids {
                if self
                    .orders
                    .get(cloid)
                    .is_some_and(|order| order.state == OrderState::PendingNew)
                {
                    self.orders
                        .set_state(cloid, OrderState::Rejected(RejectReason::Unknown));
                }
            }
        }
    }
}
