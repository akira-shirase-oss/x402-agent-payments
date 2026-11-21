# Scenario: x402-paid (the paid endpoint, M1)

The seller layer (`xpay_seller::PaymentLayer`, settle-first) in front of one paid route,
with the in-process mock facilitator doing real EIP-712 signature recovery and nonce
bookkeeping. Driven by `crates/xpay-x402-dev/src/bin/xpay-x402-load.rs` through
`scripts/load-x402.sh`: the requests are signed on every core before the clock starts, then
offered with CONCURRENCY (default 256) in flight.

Mix (per 20 requests): 14 fresh payments (a new nonce each), 2 replays of a payment
eight requests earlier (settled or still in flight), 2 unpaid requests, 1 payment whose
client hangs up right after sending it (the driver then waits until the seller ended the
claim), 1 payment from a new random signer, all of those from one client address (a
signer flood: the per-client limit sheds part of it with 503). Honest requests come from
distinct addresses. The mock facilitator answers settle after 1 ms for the whole run.

Then a second phase: 10,000 fresh payments against a store of 64 claims.

The offer's `maxTimeoutSeconds` is 3600 so the pre-signed authorisations outlive the run.
The first full run (`perf/results/x402-paid-full-20261120T201448Z.json`, kept as a miss)
used the 60 s offer: requests offered after about a minute carried expired authorisations
and were rightly refused, which the exactly-once check caught.

Totals: `MODE=smoke` 100,000 requests; `MODE=full` 1,000,000 requests.

## Thresholds (fixed before the first run)

| Check | Threshold |
| --- | --- |
| Every honest payment served exactly once (counted per payment; a replay that overtakes its original is the one served, the original gets 409 or 402) | 100% |
| 200s = honest payments | exactly equal |
| A flood payment served more than once | 0 |
| A claim still in flight after the run (hang-ups included) | 0 |
| Settlements = claims recorded Settled | exactly equal |
| 200s = claims recorded Delivered | exactly equal |
| Handler runs >= 200s | always |
| An unexpected status (a 503 to an honest client included) | 0 |
| Full-store phase: served <= 64, every other request 503, store <= 64 | always |
| p99 latency (in-process, layer + mock facilitator with 1 ms settle; hang-ups excluded) | below 50 ms |

The added rows (hang-ups, flood, latency, full store) were written before their first
run, after the review of the first M1 version.

All but the latency row are correctness under load and do not depend on the machine; only the
latency threshold does. A miss fails the run and the result is kept.

## What it does not measure

The network, a real facilitator, the chain. The facilitator's own latency is the
dominant cost in production and is bounded by the verify and settle deadlines instead.
