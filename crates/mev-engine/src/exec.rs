//! The engine → exec seam (SPEC-0010 §12, task E-6).
//!
//! The engine builds and batches **synchronously** on its own thread and hands
//! one or more [`UnsignedPost`]s to an exec backend through a non-blocking
//! [`ExecBackend::try_send`]. ECDSA signing, nonce management, and persistence
//! stay in the exec layer, off the engine thread (SPEC-0010 §12, §22). This
//! module deliberately contains no signer and no private key.
//!
//! Fail-closed contract: `try_send` returning `false` means the exec channel is
//! full or the exec task is gone. The caller must not block; it surfaces
//! [`SendError::Backpressure`] so the loop can trip the `exec_backpressure`
//! breaker (E-9) and halt new orders (SPEC-0010 §16).

use mev_hl_client::Action;
use smallvec::SmallVec;

use crate::channels::Outbound;
use crate::orders::OrderManager;
use crate::types::{Cloid, PostResult};

/// A fully-built but unsigned action the engine hands to the exec layer.
///
/// `action` is the venue-ready action (cancels, orders). `cloids` are the
/// engine-side ids carried by the action's orders, in the same order, so the
/// exec layer can route per-order statuses back by `req_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsignedPost {
    /// Request id assigned by [`ReqIds`] (monotonic per engine process).
    pub req_id: u64,
    /// The venue action to sign and send.
    pub action: Action,
    /// Engine cloids carried by the action's orders, in action order.
    pub cloids: SmallVec<[Cloid; 8]>,
}

/// A non-blocking sink for unsigned posts.
///
/// Implementations must never block: `false` means the backend is full or down,
/// and the caller fails closed (SPEC-0010 §5, §16).
pub trait ExecBackend {
    /// Try to enqueue a post; `false` means full/down.
    fn try_send(&mut self, post: UnsignedPost) -> bool;
}

impl ExecBackend for Outbound<UnsignedPost> {
    fn try_send(&mut self, post: UnsignedPost) -> bool {
        Outbound::try_send(self, post)
    }
}

/// A monotonic request-id counter owned by the engine thread.
///
/// `req_id` is the only handle the account stream needs to route a
/// [`PostResult`] back to the cloids it was built for (SPEC-0010 §10, §12).
#[derive(Debug, Clone, Default)]
pub struct ReqIds {
    next: u64,
}

impl ReqIds {
    /// A fresh counter starting at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// The next request id, advancing the counter.
    ///
    /// Monotonic by wrapping addition so a process that runs long enough to
    /// exhaust `u64` does not panic on the hot path. Named `next` by the E-6
    /// interface; it is a counter, not an `Iterator`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }

    /// The id [`Self::next`] would return, without advancing.
    pub fn peek(&self) -> u64 {
        self.next
    }
}

/// The result of dispatching one built batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Every post was handed to exec; carries the request ids sent, in order.
    Sent(Vec<u64>),
    /// Exec was full or down; the caller trips the breaker (SPEC-0010 §16).
    Backpressure,
}

impl BatchOutcome {
    /// The request ids, if the batch was sent.
    pub fn sent(&self) -> Option<&[u64]> {
        match self {
            BatchOutcome::Sent(req_ids) => Some(req_ids),
            BatchOutcome::Backpressure => None,
        }
    }

    /// Whether the batch was rejected by backpressure.
    pub fn is_backpressure(&self) -> bool {
        matches!(self, BatchOutcome::Backpressure)
    }
}

/// Why a batch could not be dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The exec channel was full or the exec task was down (fail closed).
    Backpressure,
    /// No exec backend is configured (e.g. `observe` mode).
    NoBackend,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Backpressure => f.write_str("exec backpressure"),
            SendError::NoBackend => f.write_str("no exec backend"),
        }
    }
}

impl std::error::Error for SendError {}

/// Send a built batch, registering each post's cloids only once it is accepted.
///
/// On success every post is handed to `backend` and its cloids are registered
/// with `orders` under the post's `req_id`; the returned ids are in post order.
/// On backpressure the failing post (and every later one) is **not** registered,
/// so no post is ever tracked as sent when it was not (SPEC-0010 §16). Orders
/// already inserted by the caller stay `PendingNew`; the caller (E-9) marks them
/// and trips the breaker.
pub fn dispatch<B: ExecBackend>(
    batch: crate::builder::BuiltBatch,
    backend: &mut B,
    orders: &mut OrderManager,
) -> Result<Vec<u64>, SendError> {
    let mut sent = Vec::with_capacity(batch.posts.len());
    for post in batch.posts {
        let req_id = post.req_id;
        let cloids = post.cloids.clone();
        if backend.try_send(post) {
            orders.assign_req(req_id, &cloids);
            for cloid in &cloids {
                if let Some(order) = orders.get_mut(*cloid) {
                    order.req_id = Some(req_id);
                }
            }
            sent.push(req_id);
        } else {
            return Err(SendError::Backpressure);
        }
    }
    Ok(sent)
}

/// [`BatchOutcome`]-flavoured wrapper around [`dispatch`].
pub fn dispatch_batch<B: ExecBackend>(
    batch: crate::builder::BuiltBatch,
    backend: &mut B,
    orders: &mut OrderManager,
) -> BatchOutcome {
    match dispatch(batch, backend, orders) {
        Ok(req_ids) => BatchOutcome::Sent(req_ids),
        Err(_) => BatchOutcome::Backpressure,
    }
}

/// Route a `PostAck`'s per-order statuses to the cloids it was built for.
///
/// `PostResult::Error` carries no per-order statuses, so it only clears the
/// request mapping for the caller to resolve (E-8/E-9 handle unknown outcomes).
pub fn apply_post_ack(orders: &mut OrderManager, req_id: u64, result: &PostResult) {
    match result {
        PostResult::Statuses(statuses) => orders.on_post_ack(req_id, statuses),
        PostResult::Error(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use mev_hl_client::Action;
    use mev_strategy::StrategyId;
    use rust_decimal::Decimal;
    use smallvec::smallvec;

    use super::*;
    use crate::builder::BuiltBatch;
    use crate::orders::{LiveOrder, OrderState};
    use crate::types::{CoinId, Side, VenueOrderStatus};

    fn cloid(n: u8) -> Cloid {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        Cloid(bytes)
    }

    fn live(cloid: Cloid, state: OrderState) -> LiveOrder {
        LiveOrder {
            cloid,
            coin: CoinId(0),
            side: Side::Buy,
            px: Decimal::from(100),
            sz: Decimal::ONE,
            filled_sz: Decimal::ZERO,
            reduce_only: false,
            strategy: StrategyId::from("t"),
            state,
            req_id: None,
            oid: None,
        }
    }

    fn batch_with(cloids: smallvec::SmallVec<[Cloid; 8]>) -> BuiltBatch {
        BuiltBatch {
            posts: vec![UnsignedPost {
                req_id: 7,
                action: Action::CancelByCloid { cancels: vec![] },
                cloids,
            }],
            dropped: Vec::new(),
        }
    }

    #[test]
    fn req_ids_are_monotonic() {
        let mut ids = ReqIds::new();
        assert_eq!(ids.peek(), 0);
        assert_eq!(ids.next(), 0);
        assert_eq!(ids.next(), 1);
        assert_eq!(ids.peek(), 2);
    }

    #[test]
    fn post_ack_routes_statuses_to_cloids_in_order() {
        let mut orders = OrderManager::new(1);
        let c1 = cloid(1);
        let c2 = cloid(2);
        orders.insert(live(c1, OrderState::PendingNew));
        orders.insert(live(c2, OrderState::PendingNew));
        orders.assign_req(3, &[c1, c2]);

        let result = PostResult::Statuses(smallvec![
            VenueOrderStatus::Resting,
            VenueOrderStatus::Filled,
        ]);
        apply_post_ack(&mut orders, 3, &result);

        assert_eq!(orders.get(c1).unwrap().state, OrderState::Resting);
        assert_eq!(orders.get(c2).unwrap().state, OrderState::Filled);
    }

    #[test]
    fn error_post_ack_is_ignored() {
        let mut orders = OrderManager::new(1);
        let c1 = cloid(1);
        orders.insert(live(c1, OrderState::PendingNew));
        orders.assign_req(3, &[c1]);
        apply_post_ack(&mut orders, 3, &PostResult::Error("boom".into()));
        assert_eq!(orders.get(c1).unwrap().state, OrderState::PendingNew);
    }

    #[test]
    fn dispatch_registers_on_success_and_sets_req_id() {
        struct AlwaysOk;
        impl ExecBackend for AlwaysOk {
            fn try_send(&mut self, _post: UnsignedPost) -> bool {
                true
            }
        }

        let c1 = cloid(1);
        let mut orders = OrderManager::new(1);
        orders.insert(live(c1, OrderState::PendingNew));
        let batch = batch_with(smallvec![c1]);

        let sent = dispatch(batch, &mut AlwaysOk, &mut orders).unwrap();
        assert_eq!(sent, vec![7]);
        assert_eq!(orders.get(c1).unwrap().req_id, Some(7));
    }

    #[test]
    fn backpressure_fails_closed_and_registers_nothing() {
        struct AlwaysFull;
        impl ExecBackend for AlwaysFull {
            fn try_send(&mut self, _post: UnsignedPost) -> bool {
                false
            }
        }

        let c1 = cloid(1);
        let mut orders = OrderManager::new(1);
        orders.insert(live(c1, OrderState::PendingNew));
        let batch = batch_with(smallvec![c1]);

        let err = dispatch(batch, &mut AlwaysFull, &mut orders).unwrap_err();
        assert_eq!(err, SendError::Backpressure);
        assert_eq!(orders.get(c1).unwrap().req_id, None);
    }

    #[test]
    fn dispatch_batch_reports_backpressure() {
        struct AlwaysFull;
        impl ExecBackend for AlwaysFull {
            fn try_send(&mut self, _post: UnsignedPost) -> bool {
                false
            }
        }

        let mut orders = OrderManager::new(1);
        let outcome = dispatch_batch(
            batch_with(smallvec![cloid(1)]),
            &mut AlwaysFull,
            &mut orders,
        );
        assert!(outcome.is_backpressure());
        assert_eq!(outcome.sent(), None);
    }
}
