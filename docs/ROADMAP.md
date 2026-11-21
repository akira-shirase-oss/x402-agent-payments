# x402 Agent Payments — roadmap

M0 is this base setup; the later milestones come from the end goal (`docs/END_GOAL.md`,
mapped from the spec's weeks). A milestone's status cell says *Done* only with named
tests and evidence.

| M | Spec week | Deliverable | Gate | Status |
|---|---|---|---|---|
| M0 | — | Base setup from the starter template: CI, gate, port band 63300–63399, drills and evidence kit | CI green on the empty product | **In progress — this base setup** |
| M1 | 1 | Types, seller layer, buyer client; one paid endpoint on Base Sepolia through the x402.org facilitator | 402 → pay → 200 from a Rust binary; conformance in both directions | **Offline done** (`xpay-x402-loop`, `crates/xpay-seller/tests/seller.rs`, `scripts/check-x402-reference.sh`; D-004–D-016); the live Base Sepolia run (`live_base_sepolia`, needs `X402_TESTNET_KEY`) not yet run |
| M2 | 2 | Policy service, ledger schema, atomic reservations, nonce claim; the claim guard's clean-up when the runtime shuts down, and a verify-stage panic releasing | Failure cases 1–7; LT-3, LT-4 | Planned |
| M3 | 3 | Three paid APIs; the demo agent | Agent answers under budget; failure cases 8, 12, 13; LT-1, LT-2 | Planned |
| M4 | 4 | Watcher, reconciliation, dashboard | Failure cases 14–15; LT-5, LT-6 | Planned |
| M5 | 5–6 | Pay-per-crawl demo, optional own facilitator, docs site, deploy, UI audit, review | Ship gate checked | Planned |

## Tracked for M2: open findings from the M1 re-review

Each is closed only with a test that fails without its fix.

| Finding | Severity | Text | Fix |
|---|---|---|---|
| N11 | S3 | `drain_to_log` has no caller outside tests; a dropped claim survives only as a `tracing::error` line written under the store mutex. | A periodic drainer; log after releasing the lock. |
| N12 | S3 | No per-payer bound in the hot store (only `entries.len() >= capacity`): a funded payer fills it with paid-then-hung-up claims, so every buyer gets 503 until each `validBefore` passes (testnet funding is a faucet). | A per-payer bound on claims in the hot store. |
| N13 | S3 | The wallet honours `HTTP(S)_PROXY` (no `.no_proxy()` on its client): a loopback-http proof (a bearer token) can pass through an environment proxy. | `.no_proxy()` or an explicit proxy policy. |
| N14 | S3 | The payee lock works per directory (`RUNTIME_DIRECTORY` or a temp dir; `PrivateTmp` gives each unit its own `/tmp`): two units with the same `payTo` never see each other's lock. | A fixed host-wide lock path, or `lock_dir` required in production. |
| N4 (open part) | S2 | No `AuthorizationState` implementation exists, so nothing sent ever leaves the cap outside tests; the rule that it reads at the `finalized` (or `safe`) tag, never `latest`, is only the doc comment on the trait (`crates/xpay-wallet/src/lib.rs`), not enforced. A cancelled authorisation reads as spent until events are reconciled (M4). | An RPC implementation that reads at `finalized`, with a test that fails on `latest`; event-based reconcile in M4. |
| N5 (open part) | S3 | `Wallet::reconcile` has no caller, and it is sequential, each chain read bounded by `request_timeout`: one pass over 1,024 entries (`max_unresolved`) against a silent chain takes about 21 h (1,024 × 75 s). Only a connect error on every attempt releases a reservation at once; any other unknown outcome keeps it until the chain shows the authorisation expired unused. | A periodic caller; parallel, bounded reconcile with a deadline per pass; reconcile from the ledger (M2) instead of in memory. |
| N15 | S3 now; **S2 before M3 or real funds** | Wallet state is memory only (`committed`, `unresolved`): after a restart, or with a second `Wallet` on the same key, the next `get` of a URL signs a new proof at once while the old one may still settle, and `total_cap` starts again at zero (D-012, D-020). | Persist reservations and unresolved proofs in the ledger (M2), and one live wallet per key. |
| N16 | S3 | The URL key was `Url::as_str()`, fragment included, though a fragment is never sent. Fixed: the fragment is stripped (`a_fragment_is_not_another_url_and_signs_nothing_new`, limit `url-without-fragment`). Open part: query and its order count as distinct URLs while the seller binds a claim by method and path (`crates/xpay-seller/src/layer.rs`), so `/paid` and `/paid?again` are two payments for one resource (the dev loop in `crates/xpay-x402-dev` does this on purpose); documented in D-020. | Decide the key when the ledger records the resource (M2). |
| N17 | S3 now (doc fixed); **S2 before M3 or real funds** | Expired while unresolved: `get` resends only while `now < validBefore`, answers `PaymentPending` until `validBefore` + 120 s by the local clock, then signs a new proof without reading the chain; after any held outcome a later `get` pays again and is served while the seller holds the first (bounded by `total_cap`; `committed()` counts both). | Gate a new proof for a URL on the chain's answer for the old one (needs N4). |
| Tests (re-check) | S3 | Mutation gaps: the check in `Wallet::track`, the claim guard's `504`, `Redelivery::InFlight`; the settle amount's `is_none_or`; the `409` without `PAYMENT-RESPONSE`; no HTTP-level test for N10. | A test that fails without each, listed in `scripts/prove-limits.sh` where it is a limit. |

