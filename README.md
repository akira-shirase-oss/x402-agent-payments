# x402 Agent Payments

Payment rails for AI agents over the x402 protocol: recorded, explainable decisions;
atomic caps that hold under concurrency; a ledger reconciled against the chain.

> **Status: base setup only.** Nothing of the product is built yet; the code is the
> renamed Backend Platform starter template. The contract is `docs/END_GOAL.md`.

## Quick start (the template, local)

Docker, rustup (the pinned toolchain installs itself), curl, jq, python3. Everything
binds 127.0.0.1 inside this product's port band.

<!-- quickstart:begin -->
```sh
scripts/stack.sh up        # PostgreSQL 18 in Docker, published on 127.0.0.1:63332
scripts/dev.sh start       # build, create and migrate the dev database, run the server
curl -fsS http://127.0.0.1:63350/readyz
# {"database":"ok","status":"ready"}
invite=$(scripts/dev.sh invite)
scripts/dev.sh stop
```
<!-- quickstart:end -->

The accounts walk-through — sign-up, verification, a workspace, an API token — is
`docs/ACCOUNTS.md`; the operator guide is `docs/OPERATIONS.md`. `scripts/check.sh` is
the gate CI runs.

## Layout

- `crates/xpay-{http,db,accounts,server}` — the template's crates, renamed
- `deploy/`, `scripts/`, `docs/`, `perf/` — host layer, drills, operator docs, evidence kit
- `web/` — empty until the design-system decision

Licence: MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
