# Scenario: x402-paid (the paid endpoint, M1)

The seller layer (`xpay_seller::PaymentLayer`, settle-first) in front of one paid route,
with the in-process mock facilitator doing real EIP-712 signature recovery and nonce
bookkeeping. Driven by `crates/xpay-x402-dev/src/bin/xpay-x402-load.rs` through
`scripts/load-x402.sh`: the requests are signed on every core before the clock starts, then
offered with CONCURRENCY (default 256) in flight.

Mix: 80% fresh payments (a new nonce each), 10% replays of a payment eight requests earlier
(settled or still in flight), 10% unpaid requests.

The offer's `maxTimeoutSeconds` is 3600 so the pre-signed authorisations outlive the run.
The first full run (`perf/results/x402-paid-full-20261120T201448Z.json`, kept as a miss)
used the 60 s offer: requests offered after about a minute carried expired authorisations
and were rightly refused, which the exactly-once check caught.

Totals: `MODE=smoke` 100,000 requests; `MODE=full` 1,000,000 requests.

## Thresholds (fixed before the first run)

| Check | Threshold |
| --- | --- |
| Every payment served exactly once | 100% |
| Handler runs = settlements = fresh payments | exactly equal |
| A payment served twice, or never (counted per payment; a replay that overtakes its original is the one served, the original gets 409 or 402) | 0 |
| An unexpected status | 0 |
| p99 latency (in-process, layer + mock facilitator) | below 50 ms |

The first four are correctness under load and do not depend on the machine; only the
latency threshold does. A miss fails the run and the result is kept.

## What it does not measure

The network, a real facilitator, the chain. The facilitator's own latency is the
dominant cost in production and is bounded by the verify and settle deadlines instead.
