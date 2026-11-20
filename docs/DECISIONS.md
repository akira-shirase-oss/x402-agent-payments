# Decisions

Trade-offs and decisions, D-001 onward. Each entry says what was chosen and why; a
superseded decision is marked, never rewritten.

## D-001 — Base: the Backend Platform starter template

The repository begins as a copy of the Backend Platform's copy-and-own starter template —
the bounded HTTP layer, the three-role PostgreSQL split with its protections manifest,
the accounts kit, releases, restore drills, and the CI + evidence kit — renamed for this
product. The commit it came from is in `TEMPLATE_ORIGIN`; later template changes arrive
only as reviewed diffs. Nothing shares a package, service, login or database with the
template's repository or any sibling product.

## D-002 — Port band 63300-63399

Every port this repository binds on a shared development machine stays inside
63300–63399: the server on 63350, the development PostgreSQL on 63332, the template's
other defaults shifted into the band. `scripts/check-ports.sh` fails on a literal outside
the band, and the gate proves it can fail. Rationale: Linux's ephemeral range ends at
60999, so CI and a second checkout never collide.

## D-003 — Stack

Rust backend on Axum, SQLx and tokio (the template's stack); SvelteKit + TypeScript
(strict) for the web UI once the maintainer chooses this product's design system — no
`web/` product code before that. No own contracts in v1: payments go through the x402
facilitator pattern, and Solidity + Foundry would enter only if a milestone adds on-chain
code. Everything else follows `docs/END_GOAL.md`.

## D-004 — M1 crates: protocol, seller, buyer, mock, dev

Five crates, split by who depends on them: `xpay-x402` (the v2 wire types, the bounded
base64 header codec, CAIP-2, the `exact` EVM scheme with EIP-712 via alloy, the
`Facilitator` trait, an injectable clock); `xpay-seller` (the Tower layer, the nonce store,
the HTTP facilitator client); `xpay-wallet` (the buyer); `xpay-facilitator-mock` (the
offline facilitator, a library so tests use it in process); `xpay-x402-dev` (the dev
binaries: `xpay-mock-facilitator`, `xpay-x402-loop`, `xpay-x402-load`). The seller and the
buyer share only `xpay-x402`, so a buyer build carries no Axum and a seller no signer. The
names avoid `x402-types`/`x402-axum` (published by x402-rs, `docs/KNOWLEDGE.md` §6).

## D-005 — Wire types: open where the spec is open, typed where we decide

`PaymentRequirements.extra`, `PaymentPayload.payload` and `extensions` stay JSON maps on
the wire, so an unknown extension or scheme round-trips untouched; the `exact` payload is
parsed into typed alloy values (`Address`, `U256`, `B256`) only where a decision is made.
Amounts stay decimal strings on the wire (the spec's choice; no float ever). Every header
decode is bounded (`MAX_HEADER_BYTES` = 8 KiB, before base64) and its errors never echo
the input.

## D-006 — Conformance vectors: the official spec documents, pinned by commit

The official repository has no `spec/fixtures` directory at the commit used
(`x402-foundation/x402@e35bf8846f3e40f584f7235ea96c0841cb597b27`); its normative examples
are the JSON and base64 blocks in `specs/x402-specification-v2.md`,
`specs/schemes/exact/scheme_exact_evm.md` and `specs/transports-v2/http.md`.
`scripts/x402-reference/extract-spec.py` extracts them (19 vectors) with the source line and
commit into `crates/xpay-x402/tests/fixtures/reference/spec.json`; ours must parse and
re-emit them unchanged. The other direction runs the official Python SDK (`x402` 2.28.0
from PyPI, hash-pinned in `scripts/x402-reference/requirements.txt`): it parses every
vector we emit (`tests/fixtures/emitted.json`) and its exact-EVM facilitator must reach the
same verdict as ours on each, including the failures; payloads signed by the reference
client (`python-signed.json`) must verify in ours. The TypeScript SDK was not run (no
Node toolchain in the gate); Python is the reference SDK the gate can pin by hash.

## D-007 — Per-network USDC domain from a table, never one constant

`exact::USDC` maps each CAIP-2 network to its USDC address and EIP-712 domain: Base
Sepolia "USDC"/"2", Base "USD Coin"/"2" (`docs/KNOWLEDGE.md` §4). The seller's offers are
built from it (`usdc_requirements`), the seller checks a payment against its own offer
(never the buyer's copy of `extra`), and the buyer pays only an offer whose asset and
domain match the table. A network not in the table is not offered or paid.

## D-008 — Settle-first by default; the nonce is claimed before anything else

The layer refuses offline (version, scheme, network, asset, recipient, exact amount,
validity window, signature) without touching the store; then claims (asset, payer,
nonce) atomically; then verify, then settle, and only a successful settlement runs the
handler, once. Verify-then-serve can serve one payment many times (`docs/KNOWLEDGE.md` §7
correction); with the claim first, a concurrent copy of the same payment gets 409 and a
later one 402 `invalid_exact_evm_nonce_already_used`. `SettleMode::SettleAfter` exists for
cheap idempotent handlers: still claim-first, the handler's response is dropped when
settlement fails, and a handler failure releases the claim without settling.

## D-009 — A settle timeout holds the claim; only a definite "no" releases it

Outcomes are three. Settled: the claim becomes Settled. Definitely not paid (verify said
invalid, verify unreachable — verify moves no money — or settle answered
`success: false` with a reason): the claim is released, so the buyer may retry the same
authorisation. Unknown (settle deadline passed, transport error or malformed answer
during settle, `settlement_pending`, the bookkeeping task panicked): the claim is Held,
never released, the request gets 504/502, and a retry of the same proof is not served.
`NonceStore::held()` lists them for M4's reconciliation.

Amended after review: "definitely not paid" at settle is a closed list of pre-broadcast
reasons (`RELEASE_ON_SETTLE` in the layer: insufficient funds, a bad signature or
authorisation window, an unsupported scheme or network, …) and only with an empty
`transaction`; any other `success: false` — an unexpected error, a nonce already used,
anything carrying a transaction — holds the claim. Verify and settle answers are
cross-checked against the claim (payer, network, amount, a 32-byte transaction hash); a
mismatching success is held, not served. Verify, settle, the handler and the delivery run
in ONE spawned task that owns the in-flight permit and the client's slot, with a drop
guard that records the claim (Held, or paid-but-not-delivered) if the task ends early;
a client hanging up neither cancels the bookkeeping nor frees the bound. A settled
payment whose response did not reach the client is recorded `paid_but_not_delivered`
and a retry of the same proof is served once more; a retry of a delivered payment gets
409 `payment_already_settled` with its `PAYMENT-RESPONSE`, never a 402 (a buyer must not
read "money moved" as "not paid").

## D-010 — The M1 nonce store is in memory, bounded, and sheds when full

`MemoryNonceStore` holds at most `capacity` claims, indexed by expiry (a `BTreeSet`, so
a sweep touches only what expired, never the whole map). Expired delivered claims are
dropped (the chain refuses the authorisation anyway); expired Held, in-flight and
paid-but-not-delivered claims move to a bounded reconciliation list (`held()`), so they
leave the hot store without being forgotten. When the store or that list is full, new
payments get 503 with `Retry-After`. To bound retention the layer
refuses a `validBefore` beyond `now + maxTimeoutSeconds + 30 s`. It does not survive a
restart: M2's PostgreSQL claim (failure cases 1–7) replaces it behind the same
`NonceStore` trait; until then a restart forgets Held claims, which the chain's own nonce
check still protects against double payment (not against double serving).

## D-011 — Deadlines and bounds on every facilitator and wallet I/O

`HttpFacilitator`: connect and total timeouts, no redirects, an http(s) base with a host,
answers read in chunks up to 64 KiB. The layer adds its own verify (5 s) and settle (20 s)
deadlines, a handler deadline (30 s), a `max_in_flight` semaphore (256) whose permit
lives in the payment task, and a per-client limit (`max_in_flight_per_client`, 16; IPv4
address or IPv6 /64, `X-Forwarded-For` only from `trusted_proxies`) taken before any
facilitator work. The offline checks are signature-only, so a funded-or-not random
signer passes them; these two bounds are what cap facilitator calls. The wallet: a
request timeout, no redirects (so the allowlist is checked on the URL that returned the
402, `docs/KNOWLEDGE.md` §5.6), a body cap, and a bounded number of resends.

## D-012 — The buyer: exact host allowlist, two caps, one proof per 402

`xpay_wallet::Wallet` pays only an `AllowedHost { host, pay_to }` origin: `https`, or
`http` on loopback; `host:port` exact, a bare host meaning the default port (never any
port); and only to a `payTo` on that origin's list, checked before signing (failure case
1). Only an `exact`, non-zero offer on an allowed network with the table's USDC and
domain, within `max_per_payment` and what is left of `total_cap`. The amount is reserved
under a lock before signing and released if signing fails. Once the proof has been sent,
no answer from the seller releases it — whoever holds the proof can settle it, so a 402
proves nothing. The proof is tracked as `Unresolved` (bounded by
`Limits::max_unresolved`) until `Wallet::reconcile` sees, through an
`AuthorizationState` source, that `validBefore` (+120 s for clock drift) passed and
`authorizationState(from, nonce)` is still false; used means spent.
`cancelAuthorization` also sets that state, so a cancelled proof reads as spent until M4
reconciles against events. Seller reason text reaches the caller only as a plain code. A resend after a lost answer
or 5xx carries the same proof, so one 402 is paid at most once. `validAfter` is
`now − 600 s` (USDC refuses `validAfter ≥ block.timestamp`; `docs/KNOWLEDGE.md`),
`validBefore` is `now + maxTimeoutSeconds`, and `maxTimeoutSeconds` is capped by policy.
The cap is per process in M1; M2 moves it into the ledger's transaction.

## D-013 — Signatures: 65-byte EOA, low-s, v in {27, 28}

Recovery accepts exactly 65 bytes, low-s only, and v of 27/28 (amended after review: 0/1
are refused too — the token's `ecrecover` takes only 27/28, so a 0/1 proof would pass the
offline check and fail on chain); anything else — an EIP-155-style v such as 59, found by the fuzz target — is
`invalid_exact_evm_payload_signature`, because USDC's `ecrecover` would refuse it and a
second spelling of one signature is malleability. Smart-wallet (EIP-1271/6492) signatures
are not accepted in M1.

## D-014 — The mock facilitator does the real checks; faults are switches

`xpay-facilitator-mock` runs the same `exact::check` as the seller plus a nonce ledger
(the same signature settles idempotently, a different one on a used nonce is refused) and
serves `/verify`, `/settle`, `/supported` with a 64 KiB body limit. Faults —
`settle_delay`, `settle_pending`, `settle_lost_answer` (settles, then errors),
`settle_refuse`, `verify_unreachable` — are deterministic, so every failure case is a test
that depends on no clock but tokio's paused one.

## D-015 — Offline proof in the gate, the live run skipped with a notice

Everything in M1 is proven with no network: the layer tests, the fuzz target
(`xpay-seller/tests/fuzz_header.rs`, proptest on the stable toolchain), the
`xpay-x402-loop` binary over loopback HTTP, the reference conformance. The Base Sepolia run
(`xpay-x402-dev/tests/live_base_sepolia.rs`) needs `X402_TESTNET_KEY` (a funded test key,
never committed or logged) and prints `SKIPPED` without it; it is not in the gate.

## D-016 — Dependencies added for M1

alloy (`alloy-primitives` 1.7.3 with `k256`, `alloy-sol-types` 1.7.3, `alloy-signer` and
`alloy-signer-local` 2.6.1): EIP-712 hashing, signing and recovery — the task's choice and
what x402-rs builds on. `base64` 0.22.1: the header encoding. `reqwest` 0.13.5 (`json`,
`rustls-no-provider`) with `rustls` 0.23.46 (`ring`): the facilitator and buyer HTTP
client, on the same ring provider sqlx already uses, no OpenSSL. All default features off.

The alloy tree brings `crypto-bigint` 0.5.5 (via `k256` 0.13), which has RUSTSEC-2026-0387
(timing variability in operations meant to be constant-time; fixed only in 0.7.6, which no
alloy release depends on). `deny.toml` ignores that one ID with this reason: the seller and
the mock facilitator handle only public signatures; the buyer's key is the exposure, and
the leak needs an observer on the buyer's machine. It is removed when alloy moves to
`k256` 0.14. It also ignores RUSTSEC-2024-0436 (`paste`, unmaintained, no
vulnerability), a build-time proc-macro under alloy.

## D-017 — One nonce store per payTo, one instance, until M2

At-most-once service holds only inside one nonce store: two stores for one recipient (two
instances, or two route layers) could each verify and settle the same authorisation
pair, and a facilitator may answer success to an identical re-submission. Until M2's
shared PostgreSQL claim, `PaymentLayer::new` enforces it: a process-wide registry refuses
a second store for an offer's `payTo` (`ConfigError::PayToTaken`; the same store on
several routes is fine, and the entry is freed when the last layer is dropped), and with
`PaymentConfig::lock_dir` set it takes an exclusive file lock per `payTo`, so a second
process on the host refuses to start (`ConfigError::PayToLocked`). Instances on other hosts
cannot be seen from M1: one instance per recipient stays an operator rule until M2. Offers are
validated against the USDC table at construction (D-007).

## D-018 — Load scenario covers hang-ups, a signer flood, latency and a full store

`perf/scenarios/x402-paid.md` adds clients that hang up mid-payment, fresh random
signers flooding from one address, 1 ms of facilitator settle latency, and a phase
against a 64-claim store, with thresholds written before the first run.

