# Agent Payment Rails (x402): end goal

**No agent pays without a recorded, explainable decision; no cap is ever exceeded, even under concurrency; and every
payment in the ledger is matched to the chain or flagged.**

An operations layer around the x402 protocol (v2), built in Rust. It has three parts:

- **Seller side:** a Tower layer for Axum that prices routes, answers `402` with `PAYMENT-REQUIRED`, verifies and
  settles through a facilitator, serves each payment at most once, and records revenue.
- **Buyer side:** a reqwest-based agent wallet. It intercepts `402`, asks a central policy service for a decision,
  atomically reserves budget, signs an EIP-3009 USDC authorisation, retries idempotently, and records spend.
- **Ledger:** a ledger, a chain watcher and a SvelteKit dashboard reconcile local records against on-chain transfers
  and show spend, revenue, approvals and every decision with its reason.

Everything runs on Base Sepolia testnet USDC. The value added is policy, auditability, reconciliation and measured
reliability, not a new protocol.

**Origin.** The v2.0 spec (04_AI_Agent_Payment_Rails_x402, 2026-11-20) merged with the repository rules
(`akira-shirase-oss/AGENTS.md`). Facts and corrections are in `KNOWLEDGE.md` next to this file. Numbers are quoted from
the spec (marked) or proposed targets for the maintainer to confirm. None is a measurement.

## Users and the job to be done

- **API provider or publisher (seller):** "Charge software agents per request, without accounts or API keys, and see
  exactly what I earned."
- **Agent operator (buyer):** "Give each agent a budget I can trust: caps, allowlists and approvals, outside the
  model's control, with an audit trail."
- **Finance reviewer:** "Prove that what the ledger says we paid is what happened on-chain, and export it."
- **Developer evaluating the crates:** "Add paid routes to an Axum app, or a paying client to a Rust agent, in
  minutes, and trust it to interoperate with the official SDKs."

## v1 scope

- **Protocol:** x402 v2 only (`PAYMENT-REQUIRED` / `PAYMENT-SIGNATURE` / `PAYMENT-RESPONSE`, CAIP-2 networks).
  - Scheme `exact` with EIP-3009 on Base Sepolia USDC. Its EIP-712 domain is taken from `extra`, and is "USDC"/"2" on
    Base Sepolia; it differs on mainnet.
  - The `payment-identifier` extension for idempotent retries.
  - Our own `serde` types in a crate with an unclaimed name (the spec's `x402-types` and `x402-axum` are taken on
    crates.io).
- **Seller layer** (custom `Layer` + `Service`, `mem::replace` readiness pattern):
  - per-route price, `payTo`, network and asset;
  - a bounded header decode and a deadline on every facilitator call;
  - **an atomic `(payer, nonce)` claim before the handler runs**;
  - settle-first mode for costly routes and settle-after for cheap ones, chosen per route;
  - `PAYMENT-RESPONSE` and CORS `Expose-Headers`;
  - revenue events through a transactional outbox.
- **Buyer SDK** (reqwest):
  - parse the `402`;
  - evaluate the allowlist on the URL that returned the `402` (after redirects);
  - `decide()` through the policy service;
  - sign with `alloy`, with `validAfter` set in the past (USDC's check is strict);
  - retry with the same payment id;
  - map `settlement_pending` to a held reservation, never a release.
- **Policy service and ledger API:**
  - policies (per-request max, daily cap, approval threshold, allowed domains, networks and assets);
  - decisions, each with a reason;
  - reservations, whose states are `reserved → settled | released | held_unknown`;
  - an approval queue (sign only after approval, so `validBefore` cannot lapse);
  - CSV export with formula escaping.
- **Chain watcher:** follows `safe` blocks; matches `AuthorizationUsed(authorizer, nonce)` and `Transfer` to ledger rows
  by `(payer, nonce)`; raises typed reconciliation issues (`missing_onchain`, `missing_ledger`, `amount_mismatch`,
  `duplicate`); shows recent items as `pending` while unsafe.
- **Paid APIs:** summarise (LLM), SEO/GEO check, and a pay-per-crawl article page. A demo research agent answers a
  question by buying from all three under a budget.
- **Dashboard:** spend by agent, vendor and day; revenue by route; approvals; policies; the decision log;
  reconciliation; exports; facilitator and chain status.
- **Docs:** `PROTOCOL.md` (sequence diagram, a real testnet transaction link), `POLICIES.md`, `THREATS.md`,
  `ARCHITECTURE.md`, the Starlight docs site and the changelog.

## Later (each needs its own gate)

- A self-hosted facilitator: Base Sepolia only (spec: optional, weeks 5–6).
- `upto` (Permit2) metered payments; `batch-settlement`.
- Telegram approvals; PyO3 bindings; an MCP server exposing paid tools (the official SDK already ships `@x402/mcp`).
- On-chain spend limits through smart accounts. This needs ERC-1271 for EIP-3009 signatures, because USDC validates
  signers that have code (including 7702-delegated EOAs) only through ERC-1271.
- Dynamic pricing by input size (needs bounded body buffering).

## Non-goals

- Mainnet operation, or moving or holding anyone else's funds. India's PMLA regime for VDA service providers and the
  lack of OFAC screening in a self-run facilitator rule it out (`KNOWLEDGE.md` §10). A tiny mainnet test with the
  maintainer's own funds is the spec's outer limit and is an open question.
- x402 v1 (`X-PAYMENT`), which is not wire-compatible. The official v1 packages stopped at 1.2.0.
- A parallel or extended protocol.
- Cloudflare Workers deployment (`workers-rs`), which conflicts with the standard hosting rule.
- Load-testing any third party (CDP, the x402.org facilitator, public RPCs).

## Architecture in brief

```
demo-agent ── agent-wallet (reqwest) ──decide/reserve──► policy + ledger API (Axum) ◄── watcher (alloy, safe head)
     │                                                        │  Postgres: policies, decisions, reservations,
     │ PAYMENT-SIGNATURE                                      │  settlements, revenue, recon issues, outbox
     ▼                                                        ▼
paid-apis (Axum + payment Layer) ──verify/settle──► facilitator (x402.org testnet | CDP | own) ──► Base Sepolia USDC
                                                        dashboard (SvelteKit) served by the ledger API
```

Cargo workspace: `crates/{x402 types, axum layer, agent-wallet, facilitator, ledger-api, watcher, paid-apis,
demo-agent, db}`, `web/`, `perf/`, `docs/`. Per the standard rule, the ledger API serves the dashboard on the same
origin from the product's droplet, under systemd behind Caddy, with Cloudflare in front. The spec says Cloudflare Pages
and Docker Compose; see the open questions.

## Invariants

1. **Decide before sign:** no authorisation is signed without an ALLOW decision, or an approved HOLD, recorded first.
2. **Caps hold under concurrency:** for every agent and window, reserved + held_unknown + settled ≤ cap, enforced by a
   conditional `UPDATE … RETURNING` and a database `CHECK`.
3. **At-most-once service:** a `(payer, nonce)` is served at most once, across all server instances.
4. **No free resource:** a verify or settle completes before any handler runs (the x402 v2 rule).
5. **Explainable:** every 402 encounter produces exactly one decision row (paid, blocked, held or failed) with a reason
   code, append-only.
6. **Reconciled:** once safe, every ledger settlement matches exactly one on-chain authorisation and vice versa, or a
   typed issue exists.
7. **Secrets stay secret:** agent keys and payment headers never appear in logs. Hot-wallet balances are bounded and
   topped up from a treasury.
8. **Content is data:** seller-provided text (`resource.description`, `error`) is never an instruction to the agent's
   model. Allowlists and caps sit outside the model's control.

## Required failure cases (each has a test)

| # | Trigger | Required result | Proven by |
|---|---|---|---|
| 1 | The quote exceeds the per-request max; wrong asset, network or `payTo` | BLOCK before any signature is created | Table-driven policy tests |
| 2 | A domain off the allowlist, including after a redirect | BLOCK, with the final URL in the reason | `allowlist_applies_after_redirect` |
| 3 | 50 parallel requests near the daily cap (spec), and LT-4 at scale | Zero cap violations | Concurrency test + LT-4 |
| 4 | Settle fails | The reservation is released and the decision becomes `failed` | Mock facilitator |
| 5 | Settle times out or returns `settlement_pending` | The reservation is held (`held_unknown`) until the watcher decides; never released blind | `timeout_holds_reservation` |
| 6 | The same `PAYMENT-SIGNATURE` sent N times concurrently | Served once; the rest get 402 or 409 | `replay_is_served_once` + LT-3 |
| 7 | The facilitator is down, slow or malformed | A deadline-bound 5xx; the handler is not run; bounded buyer retries reuse the payment id | Fault-injection stub |
| 8 | The handler fails after settle-first | Recorded as a vendor dispute with the transaction hash; visible in the dashboard | `paid_but_not_delivered` |
| 9 | Expired `validBefore`; `validAfter = now` | Refused with the spec's error code; the client signs with `validAfter` in the past | EIP-3009 window tests on Anvil |
| 10 | Wrong EIP-712 domain ("USD Coin" on Base Sepolia) | `invalid_exact_evm_payload_signature`; per-network fixtures | Signature fixtures |
| 11 | A third party front-runs settlement with the same payload | The server's settle sees the nonce used, confirms it matches, and records it paid, not failed | Anvil front-run test |
| 12 | Prompt injection in a page or tool output urges paying a new URL | BLOCK by allowlist; the injected text is only quoted in the reason | Injection fixture through the demo agent |
| 13 | A held approval outlives the would-be `validBefore` | Signing happens after approval; stale approvals expire | Approval-expiry test |
| 14 | Injected ledger and chain mismatches of every type | 100% flagged, zero false positives; unsafe items shown `pending` | Reconciliation fixtures + LT-6 |
| 15 | Chain reorg or stall under the watcher | No flip-flopping issues; the dashboard shows "chain stalled" | Recorded fork and stalled-head fixtures |
| 16 | Oversized or malformed base64 header; a v1 `X-PAYMENT` header | 400 within a bounded size; a clear "v2 required" error | Parser tests + fuzz target |
| 17 | Agent key leak drill | Key rotated and the old key refused; the loss is bounded by the hot-wallet balance | Runbook drill |

## Ship gate

- [ ] CI green on every push: `fmt`, `clippy -D warnings`, `nextest`, `cargo-deny`, `cargo-audit`,
  `sqlx prepare --check`, coverage, SvelteKit `check`/`lint`/`build`, the architecture check, and the parser fuzz
  targets.
- [ ] **Conformance:**
  - our client against the official `@x402/*` 2.31.0 example server;
  - the official client against our layer;
  - both against the x402.org testnet facilitator in a nightly job that is not on every push (the spec says nightly
    Base Sepolia integration).
- [ ] **Load scenarios.** Each runs at least 1,000,000 units on CI (full), with results under `perf/results/` carrying
  machine, commit and `uptime`. Each uses a local stub facilitator and Anvil, never a third party. Thresholds are
  proposed; a miss is evidence, and a change needs a decision entry.
  - **LT-1 challenge-path:** 1,000,000 unpaid requests return a valid `PAYMENT-REQUIRED`. The layer's added p99 is
    reported against a no-layer baseline; proposed target ≤ 1 ms server-side.
  - **LT-2 paid-path:** 1,000,000 paid requests with the stub's latency subtracted. Zero double-serves; the
    `criterion` micro-benchmark of layer overhead is committed alongside.
  - **LT-3 replay-storm:** 1,000,000 requests drawn from 1,000 payloads at high concurrency across two server
    instances. Each payload served exactly once.
  - **LT-4 reservations:** 1,000,000 `decide`+`reserve` calls from 1,000 agents and 64 workers on Postgres. Zero cap
    violations; p99 latency recorded.
  - **LT-5 ledger-ingest:** 1,000,000 spend and revenue events with idempotency keys. Exactly-once storage; p99 ingest
    lag recorded.
  - **LT-6 reconciliation:** 1,000,000 synthetic transfers with 1% injected mismatches across four types. 100%
    detected; zero false positives.
- [ ] Real testnet: the end-to-end paid request time is recorded in nightly runs. The spec's target is under 3 s;
  report the measured distribution, not a single run.
- [ ] All 17 failure cases pass. `THREATS.md` is reviewed by someone other than the builder, and its findings are closed
  or accepted in `DECISIONS.md`.
- [ ] Docs site `agent-payments-docs` (Astro Starlight, Cloudflare Pages):
  - quick start ending in a real paid request on Base Sepolia;
  - concepts (x402 v2, facilitators, EIP-3009, policies);
  - "which API, when";
  - tokens and keys;
  - an API reference from OpenAPI;
  - SDK guides for both crates;
  - self-hosting;
  - `/changelog`;
  - examples run in CI.
- [ ] UI covers every operation:
  - agents and keys, and policies;
  - approvals, the decision log, spend and revenue;
  - reconciliation issues and resolution, exports;
  - facilitator and chain status.
  - UI audit at 1440 and 390 px, light and dark.
- [ ] Live deployment that works without the laptop:
  - paid APIs, ledger API, watcher and dashboard on the droplet; landing and docs on Pages;
  - `/healthz` and `/readyz` watched by Service Monitor;
  - nightly backup and a restore drill;
  - a seeded demo agent, so the dashboard is never empty;
  - a real testnet transaction linked from `PROTOCOL.md`.
- [ ] Overview Definition of Done:
  - README with a 60–90 s demo, architecture diagram, quick start, live URL and known limitations;
  - CI badge and coverage, and performance numbers;
  - `ARCHITECTURE.md`;
  - build post;
  - clean history, `.env.example`, MIT or Apache-2.0;
  - crates published, or ready to publish, under free names.
- [ ] The spec's metrics: 0 cap violations; 100% of injected mismatches caught; at least one external developer
  completes the quick start (recorded with their consent, no identifiers).

## Milestones (the spec's weeks, with workspace standards added)

| M | Spec week | Deliverables | Done when |
|---|---|---|---|
| M0 | before week 1 | Workspace, CI, perf harness with LT-1 at small scale, one failure drill, docs skeleton | CI green; the drill runs in CI |
| M1 | 1 | Types, seller layer, buyer client; one paid endpoint on Base Sepolia through the x402.org facilitator | 402 → pay → 200 from a Rust binary; conformance in both directions |
| M2 | 2 | Policy service, ledger schema, atomic reservations, nonce claim | Failure cases 1–7; LT-3, LT-4 |
| M3 | 3 | Three paid APIs; the demo agent | Agent answers under budget; failure cases 8, 12, 13; LT-1, LT-2 |
| M4 | 4 | Watcher, reconciliation, dashboard | Failure cases 14–15; LT-5, LT-6 |
| M5 | 5–6 | Pay-per-crawl demo, optional own facilitator, docs site, deploy, UI audit, review | Ship gate checked |

The spec's 4–6 weeks did not include six million-unit scenarios, the docs site or the review. M5 will likely run long;
that is a working estimate.

## Open questions for the maintainer

1. **Facilitator for the live demo:** the x402.org testnet facilitator (free, no key), CDP (an account and API key;
   1,000 settles a month free, then $0.001), or our own (testnet only)?
2. **Build on x402-rs or our own types?** x402-rs 2.0.2 is four months behind the official 2.31.0 and lacks Permit2 and
   `upto`. Also choose the crate-name prefix, since `x402-types` and `x402-axum` are taken.
3. **Settle-first or settle-after** as the default for paid routes?
4. **Mainnet:** allow the spec's "tiny mainnet test with your own funds" at all, given India's FIU-IND regime?
5. **Approvals:** dashboard only in v1, or Telegram (a third-party dependency)?
6. **The LLM summarise endpoint** costs real model spend per testnet "payment". Which provider, and what budget and
   rate limit?
7. **Pay-per-crawl framing:** the spec ties it to the SEO/GEO business (Uplift AI). May the public repository and docs
   name it, or is it described generically?
8. **Hosting:** same-origin droplet with systemd (workspace) or Pages + Compose (spec)? Neon or co-located Postgres?
   This product's own droplet, or a shared one?
9. **Design identity:** palette, type and shape for this product's dashboard, landing page and docs (options with
   screenshots).
10. **Cap window:** UTC calendar day or rolling 24 hours? Org-level totals in v1, or later?
11. **Agent accounts:** plain EOAs in v1, or smart accounts (ERC-1271 required for USDC)?
12. **Networks:** Base Sepolia only, or also Solana Devnet, which the same facilitators support?
