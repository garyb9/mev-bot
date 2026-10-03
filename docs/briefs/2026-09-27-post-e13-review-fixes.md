# Task brief: fixes from the review of the post-E-13 checkpoint

**Date:** 2026-09-27
**Reviewed commits:** `5919a37`, `5b99b57`, `d9e5318`, `b48babc`, `d74478f` (on top of `3f2d96c`)
**State at review:** `cargo fmt`, `clippy -D warnings`, and `test --workspace` all pass.

The engine fix (`5b99b57`) is correct and stays. The `hl` wiring commit
(`d9e5318`) activated some old account-decoder bugs on the live path and only
half-fixed GOAL §2.2 row 9. The recorder has data-integrity bugs that affect
the G1 data clock. This brief turns the findings into tasks.

## Before you start

- Read `AGENTS.md` and `docs/GOAL.md` first. All hard rules apply:
  - never run `hl run --mode live` and never set `HL_LIVE_CONFIRM`;
  - no testnet order round-trip until every T0 row is ✅;
  - no keys, no `.env`, and nothing under `data/` in a commit;
  - money math uses `rust_decimal`;
  - no `unwrap`/`expect` on network, parse, or I/O paths.
- **Coordination:** another agent is working on R-6 (`hl record`). At review
  time it had uncommitted changes in `crates/hl-arb-bot/src/main.rs`,
  `RUNBOOK.md`, `Cargo.lock`, `crates/hl-arb-bot/Cargo.toml`, and
  `crates/hl-arb-metrics/src/lib.rs`, plus new `crates/hl-arb-bot/src/record.rs` and
  `config/record.toml`. Pull and re-read before you edit those files, don't
  touch `record.rs`, and commit only your own files.
- Line numbers below are from `d74478f` and may have moved. Search for the
  function names.
- Facts marked **⚠ verify** are not confirmed. Check them against the
  Hyperliquid docs or testnet (read-only), record the source in the spec, and
  don't rely on them until then.

## Suggested split

| Agent | Tasks | Area |
|---|---|---|
| 1 (live path) | Step 0, then A → B → C → D, then E, F | `hl-arb-engine`, `hl-arb-client`, `hl-arb-bot` |
| 2 (recorder) | G, H, I | `hl-arb-recorder` only |

If only one agent is available, follow the order in the table; the recorder
tasks come after A–D.

---

## Step 0: docs commit (first, separate commit)

`docs:` commit only, with no code:

1. **GOAL §2.2:** add these rows after row 11.
   - **12:** Fills are applied twice (`userFills` and `userEvents` both carry
     them); the `userFills` snapshot is replayed on every connect; fills are
     never mapped to orders. Position and exposure go wrong, and strategies
     never see live fills. → SPEC-0002 H-3, SPEC-0010 E-8.
   - **13:** A definitive post error, or an order the venue never saw, stays
     `Unknown` forever, so the `exec_error` breaker never clears. →
     SPEC-0002 H-2.
2. **GOAL §2.2 row 9:** reword it to the remaining defect: the exec writer
   waits for a whole group of posts before taking the next one, and a lost
   reply stalls every later post, including kill-switch cancels, for the
   timeout plus the recovery calls.
3. **GOAL §2.2 "Order" paragraph:** add rows 12 and 13 before 9, all before
   any testnet run.
4. **SPEC-0010 §20, "E-13 remaining":**
   - item 4: it is no longer "Fixed". Say "partly fixed" and point to row 9.
   - item 6: add the fill defects (row 12) and the stuck-`Unknown` defect
     (row 13).
   - item 7: correct the numbers. The bench shows the legacy decode at about
     3 µs per frame (615 µs / 203 frames), not 130–260 µs. The bench compares
     the two decodes; it does not exercise `main.rs::ingest`.
5. **RUNBOOK "Kill switch":** leave it to the R-6 agent, or do it after that
   commit lands. It still says the triggers are pending. It should document
   `hl panic`, `hl resume`, `SIGUSR1`/`SIGUSR2`, and the flag file.

---

## A. Fill stream correctness (T0, new row 12)

**Problem**
- `account_stream` (`main.rs`, around line 570) subscribes to `userFills`
  **and** `userEvents`. The venue sends each fill on both channels; the
  decoder itself reads `fills` from the `user` channel (`ingest.rs:292-302`).
- `WireUserFills` (`ingest.rs:380`) ignores `isSnapshot`, so the snapshot of
  recent historical fills is applied on the first connect and again after
  every automatic reconnect by `RawWsConn`.
- `dispatch.rs:431-434` adds every fill to `position_szi`.
  `AccountUpdate::Fill` has no `tid`, so fills can't be de-duplicated.
- `fill_update` sets `cloid: None` (`ingest.rs:428`), and `on_fill` ignores
  `None`. As a result, no live fill updates the order manager or reaches the
  owning strategy.
- Positions feed the exposure check (`state.rs:194`), and the `hl` reconcile
  doesn't correct positions (E-8 remaining). A wrong position can therefore
  approve an order that is really over the cap.

**Do**
1. Take fills from **one** source. Recommended: `userFills`, because it has
   the snapshot needed for resync. Keep `userEvents` for funding,
   liquidations, and `nonUserCancel`, and stop emitting `Fill` from the `user`
   channel.
2. Add `tid` to `AccountUpdate::Fill`, and carry the snapshot flag (a field or
   a separate variant).
3. The dispatcher keeps a bounded set of fill `tid`s it has already seen.
   - **First connect:** add the snapshot's `tid`s to the set without applying
     them. Those fills are already in the starting position.
   - **Reconnect snapshot:** apply only unseen `tid`s. These are the fills
     missed during the gap, which is the resync H-3 asks for.
   - **Live fills:** skip `tid`s already seen.
   - The bound must hold a full snapshot. ⚠ verify the snapshot's maximum
     size.
4. Map each fill to its order. Keep an `oid → cloid` index in `OrderManager`,
   filled from post acks (`Resting{oid}`, `Filled{oid}`) and `orderUpdates`.
   ⚠ verify whether the wire fill carries `cloid`; if it does, use it.
5. A fill that arrives after its order's terminal status must still reach the
   owning strategy, since `orderUpdates` and `userFills` have no guaranteed
   order. Either keep pruned orders' `oid → (cloid, owner)` for a short
   window, or don't prune a `Filled` order until its fills add up to its size
   (with a timeout).

**Done when** (tests)
- The same fill arriving on both channels moves the position once.
- A first-connect snapshot doesn't move the position.
- A reconnect snapshot with one new `tid` and N already-seen ones applies only
  the new one.
- A fill for a tracked order's `oid` updates `filled_sz` and delivers
  `OrderEventKind::Fill` to the owning strategy.
- A fill that arrives after the order's "filled" update and after pruning is
  still delivered.
- There are golden decode tests for snapshot and non-snapshot `userFills`
  frames.
- The SPEC-0010 E-8 note is updated, with a separate commit for any
  behavioural spec change.

## B. Error classification and "not found" resolution (T0, new row 13; completes row 8)

**Problem**
- `submit_post` (`main.rs:702`) turns every `Err` into `PostResult::Error`,
  which marks the orders `Unknown`. That includes errors that are definitive:
  - `Error::Exchange`: the venue replied with `status: err` or
    `type: error` (rate limited, nonce rejected, …);
  - failures before the frame reached the socket (dial failure, the
    "websocket closed before send" path, request serialisation).
- `hl-arb-client` returns `Error::UnknownOutcome` exactly when the outcome is
  ambiguous.
- The recovery asks `orderStatus` once. On "not found"
  (`order_status_update` returns `None`) it `continue`s, with no retry and no
  final outcome, though H-2 requires a "not found" resolution.
- **Result:** the order stays `Unknown`, the `exec_error` breaker never clears
  (`dispatch.rs:476`), exposure stays pinned, and the dead-man switch refreshes
  until restart.
- The recovery sends a plain `AccountUpdate::OrderUpdate`. A stale
  `orderStatus` answer that arrives after a newer account-stream update can
  therefore move the order's state backwards. `reconcile::resolve_unknown`
  guards against this; the live path bypasses it.

**Do**
1. Audit `ws_exchange.rs` (`send`, `post`) and `exchange.rs` (`prepare`), and
   split the errors into two groups:
   - **Not sent, or venue said no** → the orders are `Rejected` (a new
     `PostResult` variant, or per-order `Rejected` statuses).
   - **Sent, no reliable answer** → `Unknown`.

   If one variant covers both cases today (e.g. `Error::Decode` is raised both
   before sending and when parsing the reply), split it in `hl-arb-client`.
2. Resolve "not found": retry `orderStatus` with backoff. Once the order can
   no longer land, resolve it as not placed (`Rejected`).
   - Recommended: set `expiresAfter` on order actions, so "not found after
     `expiresAfter` + margin" is definitive. `WriteCore::with_expires_after`
     exists but `hl` doesn't set it.
   - ⚠ verify the `expiresAfter` semantics (SPEC-0002 H-9) and record them.
3. Apply `orderStatus` results only while the order is still `Unknown`, e.g.
   with a dedicated `AccountUpdate` variant handled through `resolve_unknown`.
4. Run the recovery off the exec writer's path, in its own task (see C).

**Done when** (tests; use mock `ExchangeApi`/`InfoApi`)
- `Error::Exchange` → orders `Rejected`, no `Unknown`, breaker clear.
- `UnknownOutcome` → `Unknown` → `orderStatus` "open" → `Resting`, and the
  breaker clears.
- Repeated "not found" → `Rejected` after the bound, and the breaker clears.
- A stale `orderStatus` answer after a newer stream update doesn't change the
  state.
- There is still no code path that resends an order.

## C. Exec writer: keep posts flowing (T0, completes row 9)

**Problem**
- `spawn_exec_writer` gathers the queued posts and then awaits
  `join_all(...)` (`main.rs:676`) before it reads the next post.
- A post arriving just after a group has gone out waits a full round trip.
- After a lost reply, every later post waits for the 5 s
  `DEFAULT_REQUEST_TIMEOUT` plus one `orderStatus` call per order, done one
  after another. That includes the kill switch's cancel-all.
- "Cancels go out before places" depends on tokio's fair locks and is not
  tested.

**Do**
1. Keep taking posts while earlier ones are in flight, e.g. a
   `FuturesUnordered` polled in `select!` alongside `rx.recv()`.
2. Make the socket write order hold by design: sign and enqueue each post in
   order in the writer loop, and run only the reply wait concurrently.
   - This probably needs a split API in `WsExchange`: enqueue now, then return
     a future for the reply (SPEC-0002 H-1).
   - Signing stays in the exec layer, not on the engine thread.
3. Move the lost-reply recovery (B) into a spawned task.
4. Check that no lock is held across `.await` (GOAL §5.1).

**Done when** (tests)
- With a mock exchange whose first reply takes 5 s, a second post reaches the
  socket before the first reply arrives.
- For a cancel + place batch, the mock records the socket writes in channel
  order.
- A lost reply and its recovery don't delay the next post.
- The commit message states the queue-to-socket latency before and after, by
  bench or metric.

## D. Tests for the `d9e5318` wiring (T0 evidence for rows 10 and 11)

The commit added no tests for the exec writer, the account stream, the
control task, or `hl panic`/`hl resume`. To make them testable, move
`spawn_exec_writer`/`submit_post`, `account_stream`, and `control` out of
`main.rs` into a module such as `crates/hl-arb-bot/src/live.rs`. Move only; no
behaviour change in that commit.

**Done when** (tests)
- Mock WS account frames arrive on the account channel as the expected
  `AccountUpdate`s.
- Creating the flag file sends `Control::KillSwitch` within one poll.
- `hl panic` creates the file and `hl resume` removes it.
- The SPEC-0004 K-3 end-to-end test exists: in `simulate`, all orders are
  cancelled and nothing new is placed within one iteration of each trigger.
  Tick K-3 when it passes.

## E. Kill-switch resume (T1)

**Problem**
- `SIGUSR2` sends `Resume` even while the flag file exists (`main.rs:1141`).
  The next 250 ms poll trips it again, but orders can go out in between.
- `killed` is local to the control task, so a kill sent by the dead-man task
  can't be resumed with `SIGUSR2`.

**Do**
- On `SIGUSR2`, ignore the signal (and log it) if the flag file exists.
  Otherwise send `Resume` whatever the local flag says.
- Check the flag file once before the engine starts, and start killed if it
  exists.

**Done when:** a test for each of these behaviours exists.

## F. Per-breaker state (T1)

**Problem**
- `Breakers` keeps a single flag and the first label.
- If `exec_error` trips first, a later `exec_backpressure` trip is silently
  absorbed, and `clear_label("exec_error")` then clears both.
- SPEC-0004 K-4 wants each breaker's state kept separately and shown on
  `/healthz`.

**Do:** store state per breaker. `clear_label` clears only its own breaker.
`is_tripped` means "any breaker is tripped".

**Done when:** a test trips `exec_error` then `exec_backpressure`, resolves
the `Unknown` orders, and shows that `exec_backpressure` is still tripped.

## G. Recorder `verify` coverage (T1, SPEC-0008 R-7)

**Problem**
- `CoverageAcc` (`reader.rs:395-430`) counts one min-to-max span per stream,
  minus gap_start/gap_end pairs.
- The time from a crash to the restart has no gap pair, so it counts as
  covered. §5.3 says a segment without `segment_close` ends in a gap at its
  last `t_ns`.
- This coverage number is what G1's 14 days of data will be judged on.

**Do**
1. Coverage is the union of each segment's `[first_t_ns, last_t_ns]`, minus
   gaps. A crashed segment's last `t_ns` opens a gap.
2. A `gap_start{drop}` never gets a `gap_end`, so today it marks the rest of
   the day as missing. Don't guess the rule: add an open question to
   SPEC-0008 §17 on how a drop gap ends, and flag it in your final message.

**Done when** (tests)
- A crash followed by a restart hours later shows the outage as missing.
- Back-to-back clean segments show full coverage.

## H. REST snapshotter paging holes (T1, SPEC-0008 R-5)

**Problem**
- `finish_paging` (`hl_rest.rs:590`) saves the paging state right away. It
  saves it even when `sink.send` returned `false` (the envelope was dropped),
  and before the segment writer has flushed (every ≤ 5 s).
- A crash or a drop therefore leaves a `fundingHistory`/candle range that is
  never fetched again.
- Any failed request pushes that coin's next attempt out by `daily_interval`
  (24 h).

**Do**
1. A dropped envelope does not count as progress.
2. After a crash at any point, the backfill leaves no permanent hole. For
   example, resume one page back from the saved state; duplicates are fine if
   documented for P-1 to remove.
3. Retry failed requests with capped backoff (minutes, not a day).

**Done when** (tests)
- A dropped envelope doesn't advance the state.
- A restart re-fetches the last page.
- A failed request is retried within the backoff cap.

## I. Streaming segment reader (T1, SPEC-0008 R-7; blocks E-7 part 2 and P-1)

**Problem**
- `read_envelopes` reads a whole file into memory, and `merge_segments` reads
  all files. A 1 GiB segment, or a day of books, won't fit.
- `inspect` keeps every `seq` in a `BTreeSet` and lists every missing one.
- Hole detection merges runs across restarts (`seq` restarts at 0), which
  hides holes.
- `Err(_) => break` treats every zstd error as end-of-file, including in
  finished segments, so corruption goes unnoticed.

**Do**
1. Add an iterator reader (`Iterator<Item = Result<Envelope, ReaderError>>`)
   and merge files with a k-way heap.
2. Track `seq` per run (a new run starts when `seq` goes back down) and store
   holes as ranges.
3. Tolerate a truncated zstd tail only for `.crashed` files; for finished
   segments, report it as an error.

**Done when** (tests)
- A large synthetic file is read without loading it whole (the iterator is
  lazy).
- Holes spanning a restart are found.
- A corrupted finished segment is an error.
- A truncated `.crashed` file still reads up to its last full line.

---

## Minor (fix when you are already in the file)

- `SegmentWriter::try_send` logs a `warn!` for every dropped envelope. Rate-limit
  it, and have the caller count drops (`hl_rec_dropped_total`).
- `recover_crashed` matches `"{conn}-"` as a prefix, so conn `hl-ws` would
  claim `hl-ws-02`'s files. Parse the conn from the file name and compare it
  exactly.
- `OrderManager::has_unknown()` scans every order on each market event. Keep
  an incremental count, like `working_count`.
- The ingest→sidecar `try_send` drops silently. Add a metric (in the
  `hl-arb-metrics` `names` module).
- The segment-writer throughput test uses 10-byte payloads and a wall-clock
  assertion, which is flaky on a loaded CI machine. Use realistic frame sizes,
  and consider moving the floor check into a bench.

## When you finish each task

- Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  and `cargo test --workspace`. Report any failure with its output.
- Tick the spec status in the same commit as the code. Spec text changes go in
  a separate commit.
- In your final message, say what changed, how you verified it, and which
  ⚠ verify items or open questions are still open.
