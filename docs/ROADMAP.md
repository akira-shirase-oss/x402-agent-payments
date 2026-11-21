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
| N4 (open part) | — | Reconcile is still sequential, each chain read bounded by `request_timeout`; a cancelled authorisation reads as spent until events are reconciled (M4). | Parallel, bounded reconcile; event-based reconcile in M4. |
| N5 (open part) | — | Only a connect error on every attempt releases the reservation; any other unknown outcome keeps it until the chain shows the authorisation expired unused. | Reconcile from the ledger (M2) instead of in memory. |

