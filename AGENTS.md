# x402 Agent Payments — agent instructions

Payment rails for AI agents over the x402 protocol: no agent pays without a recorded,
explainable decision; no cap is ever exceeded, even under concurrency; and every payment
in the ledger is matched to the chain or flagged.

Contract: `docs/END_GOAL.md` (what done means, scope, the required failure cases and
their drills, the ship gate); the verified facts it rests on are in
`docs/KNOWLEDGE.md`. Status: **base setup only — nothing of the product is built yet**;
the running code is the renamed Backend Platform starter template.

## Read first

1. `docs/END_GOAL.md` — the contract: scope, failure cases, ship gate.
2. `docs/KNOWLEDGE.md` — verified facts and sources; re-check versions at M0.
3. `docs/DECISIONS.md` — the decisions so far (base, port band, stack).
4. `docs/ROADMAP.md` — M0 onward; a status cell says *Done* only with named tests and
   evidence.

## Non-negotiables

1. **Performance and reliability come first.** Bounded queues and bodies, deadlines on
   every I/O await, idempotent writes, crash-safe state, no blocking on async workers.
2. **Evidence over claims.** Load runs are at least 1,000,000 units and run on CI-hosted
   runners, not a laptop; results carry machine, commit and load; misses are kept, never
   re-run until green.
3. **Every feature ships** regression tests + a fault-fixture path + a load scenario in
   the same change; every fixed bug gets a test that fails without it.
4. **The port band is 63300-63399.** Server 63350, development PostgreSQL 63332;
   `scripts/check-ports.sh` fails on a literal outside the band.
5. **Caps are atomic.** Budget reservations happen in the same transaction as the
   decision they bound; the ledger reconciles against the chain, never assumes.
6. **No secrets or payloads in logs.** Typed errors (`thiserror`) in libraries, `anyhow`
   at binary boundaries; no `unwrap`/`expect` on external input.

## Verification

`scripts/check.sh` runs what CI runs — run it before every push. Local stack:
`scripts/stack.sh up` (PostgreSQL on 127.0.0.1:63332); the server binds
127.0.0.1:63350. Its credentials are public development values.

## The web UI

The design system is the maintainer's choice and has not been made. `web/` holds only a
stub: do not start a SvelteKit app until it is decided.
