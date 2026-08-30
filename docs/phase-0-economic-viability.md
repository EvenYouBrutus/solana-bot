# Phase 0 — Economic viability gate

Status: **not a profitability finding; live trading remains disabled.** Snapshot collected 2026-08-30 UTC using Jupiter's current public `lite-api.jup.ag/swap/v1/quote` endpoint and Solana public RPC.

## What was observed

The Solana protocol documentation states a base fee of 5,000 lamports per signature, and that a priority fee is charged even on failed transactions. The live public RPC `getRecentPrioritizationFees` response at slots 442910142–442910291 was zero for its unfiltered sample. That is not a usable promise of zero priority cost: public RPC is not a congestion SLO and a production transaction must query a fee estimator close to submission. The cost model must include base plus selected priority fee per attempt.

Jupiter live SOL→USDC exact-in quotes (100 bps tolerance) returned roughly $5.32 for 0.05 SOL, $10.64 for 0.10 SOL, and $26.60 for 0.25 SOL. Quoted `priceImpactPct` was approximately zero (0–0.0204 bps) for these highly liquid SOL/USDC routes. This is only a calibration/smoke test; it says nothing about micro-cap memecoin routes, whose fills are expected to be materially worse.

## Conservative model and implication

The checked-in calculator deliberately does not treat `slippageBps` tolerance as a cost. It requires a *realised* adverse-execution assumption. For a provisional micro-cap scenario — 30 bps swap fee, 100 bps realised slippage, 20 bps price impact per fill, 0.2 cents network cost per attempted swap, 10% failure rate, and 0.2 cents per failed attempt — round-trip cost is $0.3044 / 3.044% on a $10 position. With a 10% gross average loss and 2:1 gross win/loss ratio, break-even win rate is 43.48%; required average win is 9.566%.

This provisional calculation is **UNVALIDATED**. A 300 bps realised slippage plus 100 bps impact case on a $3 position crosses the 8% gate and is rejected by test. The gate therefore makes no entry when actual calibrated conditions are too expensive or unavailable.

## Operator decision

At $10–30 equity, operate one position at most; 50% maximum allocation leaves funds for exit/network costs. The present default minimum viable position is $8, meaning a $10 account may frequently have no eligible trade. That is correct behaviour, not a reason to lower the rule. A $20–30 account can only consider one $8–15 position, and only when the per-token live quote and measured historical fills pass the 8% round-trip gate and the strategy independently clears the minimum net edge.

Phase 4+ is **not unlocked**. Before strategy, paper, or execution work, collect token-specific two-sided Jupiter quotes, execution/failure samples, a source for congestion-aware priority fees, and recompute this report. If those measured costs exceed the gate for the intended size, the outcome is: do not trade at this size; use fewer/higher-conviction opportunities, increase capital, or stop the experiment.

## Sources

- Solana, [Fee Structure](https://solana.com/docs/core/fees/fee-structure) (base fee, priority-fee formula, failed-transaction charging).
- Jupiter, [Swap API quote endpoint](https://lite-api.jup.ag/swap/v1/quote) (live quote endpoint used above; parameters supplied in snapshot).
