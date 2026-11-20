# Agent Payment Rails (x402): verified domain knowledge

Checked on 2026-11-20 against the v2.0 spec (04_AI_Agent_Payment_Rails_x402, dated 2026-11-20).

How to read the labels:

- Every fact carries a source.
- **Observed** means I read it myself from a public Base RPC or a package registry on 2026-11-20.
- **UNVERIFIED** means I could not confirm it.
- **The spec says** marks numbers taken from the spec without confirmation.

None of this is legal advice.

## 1. The x402 protocol today

- **Origin and governance.** Coinbase announced x402 on 2025-05-06
  (https://www.coinbase.com/developer-platform/discover/launches/x402).
  - Coinbase and Cloudflare announced an x402 Foundation in September 2025
    (https://www.cloudflare.com/press-releases/2025/cloudflare-and-coinbase-will-launch-x402-foundation).
  - It launched under the Linux Foundation in April 2026 and later reported 40 member organisations
    (https://www.linuxfoundation.org/press/linux-foundation-announces-operational-launch-of-x402-foundation-to-standardize-internet-native-payments-for-ai-agents-and-applications).
  - **The canonical repository is now `github.com/x402-foundation/x402`** (Apache-2.0). The `coinbase/x402` README says
    the repo moved and `coinbase/x402` is "a development fork". The spec's `github.com/coinbase/x402` link is outdated.
- **Current protocol version: 2.** The version history lists "v2.0 | 2025-12-9"
  (https://github.com/x402-foundation/x402/blob/main/specs/x402-specification-v2.md).
  - v1 is not wire-compatible with v2: it used `X-PAYMENT` and `X-PAYMENT-RESPONSE`.
  - Networks are CAIP-2 identifiers, for example `eip155:84532` for Base Sepolia.
- **HTTP transport (v2)** (https://github.com/x402-foundation/x402/blob/main/specs/transports-v2/http.md). All three
  headers carry base64-encoded JSON:
  - `PAYMENT-REQUIRED` (server to client) carries the `PaymentRequired` object. Its fields are `x402Version` (2),
    `error`, `resource {url, description, mimeType}`, and `accepts[]`, each with `scheme`, `network`, `amount`,
    `asset`, `payTo`, `maxTimeoutSeconds`, `extra {name, version}`.
  - `PAYMENT-SIGNATURE` (client to server) carries the `PaymentPayload`.
  - `PAYMENT-RESPONSE` (server to client) carries the `SettlementResponse`.
  - Status codes: 402 for payment required or payment failed, 400 for an invalid payment, 500 for a server error.
  - **Spec correction:** the spec's flow diagram puts `resource` and `timeout` inside each requirement. In v2,
    `resource` is top-level and the field is `maxTimeoutSeconds`. The spec itself says to check the current spec
    ("Check before building"); this is that check.
- **Facilitator API** (core spec):
  - `POST /verify` takes `{x402Version, paymentPayload, paymentRequirements}` and returns `isValid`,
    `invalidReason`, `payer`. It "is **read-only**" and "MUST NOT commit payment state or write onchain state".
  - `POST /settle` returns `success`, `errorReason`, `transaction`, `network`, `amount`.
  - `GET /supported` lists `kinds`, `extensions` and `signers`.
  - Clients "MUST use the corresponding `invalidReason` or `errorReason` code and MUST NOT rely on message text".
  - `settlement_pending` is non-terminal and "MUST carry a non-empty `transaction`".
- **Ordering.** The default `authorization` flow is "verify → resource → settle → respond". `upfront` is "settle →
  resource → respond", and `escrow` is "settle → resource → settle → respond". The invariant: at least one verify or
  settle "MUST run before the resource executes".
- **Extensions** in the repository (`specs/extensions/`; listed via the GitHub API): `bazaar` (discovery),
  `payment_identifier`, `sign-in-with-x`, `eip2612_gas_sponsoring`, `erc20_gas_sponsoring`,
  `extension-offer-and-receipt`, `http-message-signatures`, `builder_code`, `extension-auth-hints`.
  - **`payment-identifier`** is idempotency for x402. The client sets a 16–128-character `id` and reuses it on
    retries. The same `id` with the same payload returns the cached response; the same `id` with a different payload
    returns `409 Conflict`. Servers "should bind each `id` to a normalized request fingerprint"
    (https://github.com/x402-foundation/x402/blob/main/specs/extensions/payment_identifier.md). Its maturity status is
    **UNVERIFIED**.

## 2. Schemes and networks

- Scheme directories: `exact`, `upto`, `batch-settlement`, `auth-capture`. `exact` has per-chain documents for EVM,
  SVM, Aptos, Stellar, Sui, TON, XRPL, Starknet, NEAR, Hedera and **`lnbtc` (Lightning)**, among others.
  - The overview's "Lightning as an alternative rail" idea already has a scheme document. Its maturity is
    **UNVERIFIED**.
- **`exact` on EVM** (https://github.com/x402-foundation/x402/blob/main/specs/schemes/exact/scheme_exact_evm.md):
  - Three transfer methods: EIP-3009 `transferWithAuthorization` ("Recommended"); Permit2 through
    `x402ExactPermit2Proxy` for any ERC-20 (the "Universal Fallback"); ERC-7710 delegation for smart accounts.
  - "If no `assetTransferMethod` is specified … clients should default to `"eip3009"`".
  - The facilitator checks the signer, the balance, the amount and validity window, the token and network, then
    simulates the transfer.
  - The facilitator "cannot modify the amount or destination".
- **`upto` on EVM** is Permit2 only, because "EIP-3009 … requires exact amounts at signing time". The client signs a
  ceiling, and the server settles the metered amount, which may be zero. "Malicious servers could charge up to `amount`
  regardless of actual usage"
  (https://github.com/x402-foundation/x402/blob/main/specs/schemes/upto/scheme_upto_evm.md). This is where the spec's
  "streaming/metered payments" stretch goal belongs.
- **CDP facilitator networks** (https://docs.cdp.coinbase.com/x402/network-support):
  - mainnet: Base, Polygon, Arbitrum, Monad, World, Solana;
  - testnet: **Base Sepolia (`eip155:84532`)**, World Sepolia, Solana Devnet;
  - schemes: `exact`, `upto`, `batch-settlement` on EVM; "all ERC-20 tokens … through EIP-3009 or Permit2".

## 3. The facilitator role

- The facilitator validates signed payments, submits settlement on-chain, and reports the result. CDP's version also
  runs "OFAC and Know Your Transaction (KYT) checks" and authenticates with a CDP API key ID and secret
  (https://docs.cdp.coinbase.com/x402/core-concepts/facilitator).
- **Cost.** "The first 1,000 onchain Facilitator transactions each month are free", then $0.001 per on-chain
  transaction; "Payment verification is always free" (https://docs.cdp.coinbase.com/x402/network-support). Only
  successful settles are billed (https://docs.cdp.coinbase.com/x402/support/faq).
- **Testnet.** The public `https://x402.org/facilitator` is free, needs no key, and covers Base Sepolia and Solana
  Devnet (CDP FAQ above). The x402 README warns: "Do not assume the public x402.org facilitator is the default
  production path for mainnet EVM routes".
- **A self-hosted facilitator** needs a funded gas wallet, exact simulation before submission, and handling of
  `settlement_pending`. It has no OFAC screening unless you add it.

## 4. EIP-3009 and USDC

- **EIP-3009 is still "Draft"** (Standards Track: ERC, created 2020-09-28; https://eips.ethereum.org/EIPS/eip-3009).
  - It defines `transferWithAuthorization`, `receiveWithAuthorization`, `cancelAuthorization` and
    `authorizationState(authorizer, nonce)`.
  - Nonces are random 32-byte values, not sequential, so many authorisations can be outstanding at once.
  - Its security section: "Use receiveWithAuthorization instead of transferWithAuthorization when calling from other
    smart contracts". A watcher can extract a transfer authorisation from the mempool and submit it.
  - x402 uses `transferWithAuthorization`, so anyone holding a payload can settle it, but only to the signed `to`.
- **USDC's implementation** (Circle `stablecoin-evm`):
  - `_requireValidAuthorization` uses strict inequalities, `now > validAfter` and `now < validBefore`, so a payload
    with `validAfter = now` fails in the same second
    (https://github.com/circlefin/stablecoin-evm/blob/master/contracts/v2/EIP3009.sol).
  - Each use emits `AuthorizationUsed(authorizer, nonce)`.
  - The v,r,s overload repacks to bytes and goes through `SignatureChecker.isValidSignatureNow` (same file).
- **The EIP-7702 / smart-wallet trap.** `SignatureChecker` uses ECDSA only `if (!isContract(signer))`. Otherwise it
  uses ERC-1271 only
  (https://github.com/circlefin/stablecoin-evm/blob/master/contracts/util/SignatureChecker.sol). An agent EOA that has
  delegated code through EIP-7702 must implement ERC-1271, or its EIP-3009 signatures fail. FiatToken v2.2 introduced
  the bytes-signature form (https://www.circle.com/blog/announcing-usdc-v2-2).
- **Observed on 2026-11-20:**

  | Network | USDC address | `name()` | `version()` | Bytes-signature selector `0xcf092995` |
  |---|---|---|---|---|
  | Base Sepolia (84532) | `0x036CbD53842c5426634e7929541eC2318f3dCF7e` | "USDC" | "2" | present in the implementation bytecode |
  | Base (8453) | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` | "USD Coin" | "2" | present |

  The EIP-712 domain name differs between testnet and mainnet. Hard-coding one value breaks the other. Read
  `extra.name` and `extra.version` from the requirements, and check them against the chain.
- **Other chains:** **UNVERIFIED** here. Check each chain's USDC implementation before enabling it in the network
  allowlist.

## 5. Replay protection and payment-state pitfalls

1. **The on-chain nonce is the only hard replay guard.** `authorizationState(from, nonce)` flips once. The spec's "server
   tracks used nonces" is still needed, for a different reason:
   - `/verify` is read-only, so under the default verify → resource → settle flow, N concurrent requests carrying the
     same `PAYMENT-SIGNATURE` can all verify and all be served, while only one settles.
   - A server must claim `(payer, nonce)` atomically before running the handler, or settle first.
2. **Settle-after versus settle-first.**
   - x402-axum settles **after** the handler by default and offers `settle_before_execution`
     (https://docs.rs/x402-axum/latest/x402_axum/).
   - Settle-after risks serving a resource whose settlement then fails (the payer emptied the wallet, or the
     authorisation expired).
   - With Base now producing 200 ms blocks (§8), settle-first costs about one block plus facilitator round trips.
   - Recommendation (inference): settle-first for costly handlers such as LLM summarisation.
3. **Unknown outcomes.** A settle that times out or returns `settlement_pending` is neither success nor failure. The
   buyer must keep the spend reservation held until reconciliation decides. Releasing it on a timeout lets parallel
   requests exceed the cap. The spec says "Reservation released if settlement fails or times out", which is wrong for
   the timeout case.
4. **Validity window versus approvals.** A payment held for human approval (spec P1) can outlive `validBefore`. Sign
   only after approval, or re-sign after it.
5. **Front-running.** A third party who sees the header can settle the authorisation first. The funds still reach
   `payTo`, but the server's own settle then fails with "authorization is used". Treat "already used, and it matches
   this payment" as paid, idempotently.
6. **Redirects.** reqwest follows redirects by default (UNVERIFIED for 0.13's default policy). The buyer must run the
   domain allowlist against the URL that returned the 402, not the URL it first requested.
   M1: the wallet and the facilitator client disable redirects (`redirect::Policy::none()`), so the URL that returned
   the 402 is the one requested and allowlisted.

## 6. Rust implementations

- **The official repository has no Rust SDK.** Its install docs cover TypeScript (`@x402/*`), Python (`pip install
  x402`) and Go, and there is a `java/` directory (https://github.com/x402-foundation/x402).
  - Observed versions: `@x402/core`, `@x402/evm`, `@x402/express`, `@x402/fetch` and `@x402/hono` are 2.31.0
    (2026-11-12); the Go module is v2.31.0; PyPI `x402` is 2.28.0.
  - Re-checked at M1: the repository at commit `e35bf8846f3e40f584f7235ea96c0841cb597b27` carries `@x402/core` 2.32.0;
    PyPI `x402` is still 2.28.0, which the conformance gate pins.
  - The legacy v1 npm packages `x402` and `x402-express` stopped at 1.2.0 (2026-04-16).
  - The official TypeScript SDK already ships `@x402/mcp`, which overlaps the spec's MCP stretch goal.
- **x402-rs** (community, Apache-2.0, https://github.com/x402-rs/x402-rs):
  - README: supports "protocol versions (V1 and V2)"; only `exact` is implemented; Upto, Deferred and Permit2
    ("Gasless Approval Flow") are planned. Observed: 60 open issues, last push 2026-07-13.
  - Crates, all 2.0.2 (July 2026): `x402-types`, `x402-axum`, `x402-reqwest`, `x402-facilitator-local`,
    `x402-chain-eip155`, `x402-chain-solana`. The facilitator server binary is not on crates.io.
  - Its EVM crate depends on alloy `^2.0` and `alloy-primitives ^1.6`, so it is compatible with current alloy 2.6.1.
  - It lags the official SDK by four months and lacks Permit2 and `upto`. Conformance tests against `@x402/*` 2.31.0
    are essential.
  - Other crates exist, for example `r402` 0.21.0 (2026-09-05) (crates.io search).
- **Crate-name collision (spec correction):** `x402-types` and `x402-axum` are already published by x402-rs. The
  spec's crate names cannot be published as written.
- **Versions to plan for** (crates.io, observed):

  | Crate | Version | Notes |
  |---|---|---|
  | `alloy` | 2.6.1 | MSRV 1.94.1 |
  | `axum` | 0.8.9 | |
  | `tower` | 0.5.3 | |
  | `tower-http` | 0.7.1 | |
  | `reqwest` | 0.13.5 | |
  | `sqlx` | 0.9.1 | MSRV 1.94.0 |
  | `tower_governor` | 0.8.0 | last release 2025-08-14 |

## 7. Tower middleware patterns for the seller layer

- **Choice of form.** axum lists `from_fn`, `from_extractor`, tower combinators, a boxed-future `Service`, and a
  hand-written future (https://docs.rs/axum/latest/axum/middleware/index.html). A publishable, configurable payment
  layer is a custom `Layer` plus `Service` with a boxed future.
- **Readiness.** "Services are permitted to panic if `call` is invoked without obtaining `Poll::Ready(Ok(()))` from
  `poll_ready`." When the future needs the inner service, clone it and take the one that was made ready with
  `std::mem::replace(&mut self.inner, clone)` (https://docs.rs/tower/latest/tower/trait.Service.html).
- **Ordering.**
  - With `Router::layer`, middleware runs bottom to top; `ServiceBuilder` composes top to bottom (axum docs).
  - Put tracing and the request ID outermost, then the timeout, rate limit and body limit, then the payment layer,
    then the handler. A payment must never be verified for a request that the rate limiter will refuse.
- **Passing state.** "State can be passed from middleware to handlers using request extensions." These are "not
  automatically moved to response extensions" (axum docs). Put the verified payer and amount in an extension, and set
  `PAYMENT-RESPONSE` on the way out.
- **Browsers.** The v2 transport document has a CORS section. Browser clients can read `PAYMENT-REQUIRED` and
  `PAYMENT-RESPONSE` only if they are listed in `Access-Control-Expose-Headers`.
- **Bounded everything.**
  - A deadline on each facilitator call. A facilitator that is down returns 5xx and never serves the resource.
  - A size limit on decoding the base64 header.
  - No body buffering, except for P2 dynamic pricing by input size, and then with a limit.

## 8. The chain: Base Sepolia

- **Block time (spec assumption outdated).** Base's Denim activation (Base Sepolia 2026-10-21, mainnet 2026-11-05)
  replaced Flashblocks "with a complete canonical block every 200ms"
  (https://docs.base.org/base-chain/flashblocks/faq). Observed: 0.2 s per block on both networks.
- **Finality.** `safe` lagged `latest` by about 63 s and `finalized` by about 16 minutes on Base Sepolia (observed).
  The OP Stack can reorganise unsafe blocks until their data reaches Ethereum
  (https://docs.optimism.io/op-stack/transactions/transaction-finality). The watcher should reconcile against `safe`
  and mark recent items "pending", not "mismatch".
- **Reconciliation key.** Each EIP-3009 payment emits `AuthorizationUsed(authorizer, nonce)` plus `Transfer`. Match
  ledger rows on `(payer, nonce)`, not on amount and time.
- **Test funds.** Circle's faucet gives 20 USDC per address per chain every 2 hours, Base Sepolia included
  (https://faucet.circle.com/).

## 9. Competition and the pay-per-crawl idea

- **Spending controls already exist (spec correction).** The spec's "companies giving agents budgets have no standard
  controls" is outdated:
  - CDP's SDK has x402 "client-side spend controls" covering per-payment caps, rolling limits, and network, asset and
    payee allowlists (https://docs.cdp.coinbase.com/get-started/changelog);
  - Coinbase's Agentic Wallets (2026-02-11) advertise session caps and per-transaction limits
    (https://www.coinbase.com/developer-platform/discover/launches/agentic-wallets).
  The differentiator has to be the server-side policy store, an auditable decision log, and reconciliation.
- **Cloudflare pay per crawl** uses HTTP 402 with its own `crawler-price`, `crawler-exact-price`/`crawler-max-price` and
  `crawler-charged` headers, and Web Bot Auth signatures. It is not x402
  (https://developers.cloudflare.com/ai-crawl-control/features/pay-per-crawl/use-pay-per-crawl-as-ai-owner/crawl-pages/).
- **Cloudflare's Monetization Gateway** (announced 2026-07-01, waitlist) will "charge for any asset protected by
  Cloudflare" and settle over x402 (https://blog.cloudflare.com/monetization-gateway/). It is a direct competitor to the
  spec's seller kit and pay-per-crawl demo for any site behind Cloudflare.
- **Card fees** (the spec says ~$0.30 + %): Stripe's page served here showed the Canadian "2.9% + CA$0.30". The US rate
  is **UNVERIFIED** from this location (https://stripe.com/pricing).

## 10. Regulatory constraints on a public demo (not legal advice)

- **Testnet only** for anything the public can trigger. The spec's own rule ("tiny mainnet test only with your own
  funds") is the outer limit.
- **India.** VDA service providers that transfer or safekeep virtual digital assets "for or on behalf of" another person
  in business must register with FIU-IND under PMLA notification S.O. 1072(E) of 2023-03-07
  (https://pib.gov.in/PressReleasePage.aspx?PRID=1991372; the activity list is summarised at
  https://www.jsalaw.com/wp-content/uploads/2023/03/JSA-Prism-AWCCI-PMLA.March-2023.Final0782.pdf).
  - **Constraint:** never run a mainnet facilitator, treasury or hot wallet that moves third parties' funds.
  - Whether a non-custodial facilitator counts is **UNVERIFIED**.
- **Sanctions screening.** The CDP facilitator screens (OFAC/KYT); a self-hosted one does not. **Constraint:** a
  self-hosted facilitator stays on testnet.
- **US money-transmission treatment** of facilitators: **UNVERIFIED**.

## 11. Corrections to the spec, in brief

1. The repository moved to `x402-foundation/x402`; the protocol is v2 with `PAYMENT-*` headers and CAIP-2 networks.
2. Timing out a settle must not release a reservation.
3. Verify-then-serve allows concurrent double-serve; claim the nonce atomically, or settle first.
4. Base has 200 ms blocks.
5. The USDC EIP-712 name differs between Base Sepolia ("USDC") and Base ("USD Coin").
6. 7702-delegated agent wallets need ERC-1271.
7. The spec's crate names are taken on crates.io.
8. Spend controls and an x402 edge gateway already exist from Coinbase and Cloudflare.
9. `upto` (Permit2) and `payment-identifier` already cover the spec's metered-payment and idempotency needs.
