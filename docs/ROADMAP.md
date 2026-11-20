# x402 Agent Payments — roadmap

M0 is this base setup; the later milestones come from the end goal (`docs/END_GOAL.md`,
mapped from the spec's weeks). A milestone's status cell says *Done* only with named
tests and evidence.

| M | Spec week | Deliverable | Gate | Status |
|---|---|---|---|---|
| M0 | — | Base setup from the starter template: CI, gate, port band 63300–63399, drills and evidence kit | CI green on the empty product | **In progress — this base setup** |
| M1 | 1 | Types, seller layer, buyer client; one paid endpoint on Base Sepolia through the x402.org facilitator | 402 → pay → 200 from a Rust binary; conformance in both directions | Planned |
| M2 | 2 | Policy service, ledger schema, atomic reservations, nonce claim | Failure cases 1–7; LT-3, LT-4 | Planned |
| M3 | 3 | Three paid APIs; the demo agent | Agent answers under budget; failure cases 8, 12, 13; LT-1, LT-2 | Planned |
| M4 | 4 | Watcher, reconciliation, dashboard | Failure cases 14–15; LT-5, LT-6 | Planned |
| M5 | 5–6 | Pay-per-crawl demo, optional own facilitator, docs site, deploy, UI audit, review | Ship gate checked | Planned |
