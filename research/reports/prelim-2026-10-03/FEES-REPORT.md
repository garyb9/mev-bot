> Written by the orchestrator from subagent hand-backs; UNVERIFIED items are marked.

# HL fee research for S-5 (fetched 2026-10-03, primary: hyperliquid.gitbook.io)

Written by the orchestrator from the subagent's hand-back. Pages: /trading/fees, /for-developers/api/rate-limits-and-user-limits, /for-developers/api/exchange-endpoint, /for-developers/api/hip-3-deployer-actions, /referrals. WebFetch summarised the pages with a small model, so numbers match the returned text but raw HTML was not seen. UNVERIFIED marks items not confirmed.

## Fee schedule
Volume is rolling 14-day, assessed daily UTC; weighted = perps + 2 x spot. One tier across perps, HIP-3 perps and spot.

| Tier | 14d weighted vol | Perp taker/maker bps | Spot taker/maker bps |
|---|---|---|---|
| 0 | base | 4.5 / 1.5 | 7.0 / 4.0 |
| 1 | >$5M | 4.0 / 1.2 | 6.0 / 3.0 |
| 2 | >$25M | 3.5 / 0.8 | 5.0 / 2.0 |
| 3 | >$100M | 3.0 / 0.4 | 4.0 / 1.0 |
| 4 | >$500M | 2.8 / 0.0 | 3.5 / 0.0 |
| 5 | >$2B | 2.6 / 0.0 | 3.0 / 0.0 |
| 6 | >$7B | 2.4 / 0.0 | 2.5 / 0.0 |

Maker rebates by share of 14d weighted maker volume: >0.5% -0.1 bps, >1.5% -0.2, >3.0% -0.3 (denominator UNVERIFIED; negligible anyway). Stable-pair spot: 80% lower taker. Aligned quote assets: 20% lower taker.

## Staking and referral
HYPE staking discount: >10 5%, >100 10%, >1,000 15%, >10,000 20%, >100,000 30%, >500,000 40% (applies to rebates: UNVERIFIED). Referral: 4% off fees on first $25M of own volume (trivial); referral code needs $10,000 of own volume.

## HIP-3
Growth mode cuts protocol fees, rebates, volume credit and rate-limit contribution by 90% (taker ~0.45-0.9 bps). Deployer scale 0-3 (0-10 growth). xyz, io, mkts scale 1.0 (observed live 2026-09-28); para: costs.toml says 0.5, the recorder saw 1.0 (conflict to resolve). costs.toml formula scaleIfHip3 is UNVERIFIED against the doc table. Growth mode does not help climb tiers.

## S-5 break-even (taker both legs, RT = 2 x taker; need RT <= ~2-4 bps against ~4 bps gross)
- Tier 0, no staking: 9.0 RT (the study's number). Tier 0 + Diamond: 5.4. Tier 2 + Diamond: 4.2. Tier 3 + Diamond: 3.6. Tier 6 + Diamond: 2.9.
- Minimum to get under 4 bps: Diamond (>500,000 HYPE staked) plus >= ~$100M/14d weighted volume. Unrealistic for a new account; volume of that size at 9 bps RT loses ~$900 per $1M.
- Maker both legs: 3.0 RT at tier 0, but adverse selection and queue position mean fills skew to losers and the 4 bps gross was measured with taker fills. A maker study needs the trades stream plus queue modelling (compete_usd). Do not assume 3 bps is achievable.

## Small-account blockers
- scheduleCancel: time >= 5 s ahead, max 10 triggers/day (reset 00:00 UTC), omit time to remove. The CURRENT docs text contains NO volume requirement; the ~$1M rule is UNVERIFIED (test on testnet).
- Minimum order value $10 (secondhand via summariser, probable).
- Address rate limit: 1 request per 1 USDC cumulatively traded, initial buffer 10,000 requests, then 1 per 10 s; a batch of n orders counts n. IP 1200 weight/min REST. Open orders 1000 + 1 per $5M volume (max 5000). Constraining for any maker/quoting variant; taker-only S-5 burns few.

## Recommendation
1. research/costs.toml: add named account scenarios (base, staked_diamond multiplier 0.6, tier_k) and a tier/staking argument in hlr.costs; multiplier = (1-staking)(1-referral).
2. S-5 grading prints break-even RT per scenario, net of the 2 bps buffer.
3. S-5 stays FAIL for any account we can realistically have. Only a maker-leg variant or a growth-mode HIP-3 venue could change it. S-1 improves if both legs are growth-mode HIP-3 (0.9 bps taker; S-1 already priced growth mode). S-3/S-19 priced at base spot 7.0 bps.
4. Resolve para fee scale 0.5 vs 1.0 and the scaleIfHip3 formula.
5. Record as UNVERIFIED in the spec: scheduleCancel volume gate, HYPE USD price for Diamond capital, rebate share denominator, staking discount on rebates, exact $10 minimum text.
