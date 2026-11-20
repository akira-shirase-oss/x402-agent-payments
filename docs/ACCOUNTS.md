# The accounts kit

`crates/xpay-accounts` and `migrations/0002_accounts.sql`: the slice every product's
`SAAS_PLAN.md` repeats, as copied code. Its tables live in the product's own database and its
routes mount in the product's own router. It is never a shared login service (Backend
Platform D-011): identity across products is at most an OIDC *contract*, and the kit does not
implement OIDC.

## Routes (under `/v1`)

| Route | Who | What |
|---|---|---|
| `POST /signup` `{invite, email, password}` | anyone with an invite | 201 `{user_id, verification_email: sent\|failed}`; 403 `invite_invalid`; 409 `email_taken` (sign-up is invite-gated, so this says little); 422 `weak_password` (under 12 characters), `password_too_long` (over 512 bytes), `invalid_email` |
| `POST /verify` `{token}` | the link's holder | 204; 400 `token_invalid` (unknown, used or expired: one answer) |
| `POST /verify/resend` `{email}` | anyone | 202 always; an unverified account gets a fresh link |
| `POST /password-reset` `{email}` | anyone | 202 always; an existing account gets a link valid for an hour |
| `POST /password-reset/confirm` `{token, password}` | the link's holder | 204, and every session and trusted address of the account ends |
| `POST /session` `{email, password}` | anyone | 200 and the session cookie; 401 `sign_in_failed` (wrong password and unknown account alike, after the same work); 403 `email_unverified` (right password); 429 `rate_limited` with `Retry-After` |
| `DELETE /session` | a person | 204, cookie cleared |
| `GET /me` | a person | email, verification, workspaces with roles |
| `DELETE /me` `{password}` | a person, re-authenticated under the sign-in budgets | 204; 409 `transfer_ownership_first` while they solely own a workspace with other members |
| `POST /workspaces` `{name}` | a person | 201; 403 `plan_limit` (`owned_workspaces`) |
| `GET /workspaces/{id}` | members; the workspace's API tokens | name, plan, role |
| `DELETE /workspaces/{id}` | owners | 204; members and tokens go with it |
| `GET /workspaces/{id}/members` | members | |
| `POST /workspaces/{id}/members` `{email, role}` | admins (owners for `owner`) | a verified account joins; 404 for no such verified account; 409 `already_member`; 403 `plan_limit` (`members`) |
| `DELETE /workspaces/{id}/members/{user}` | admins, or the member themself | 409 `last_owner`; an admin cannot remove an owner |
| `GET /workspaces/{id}/tokens` | admins | id, display prefix, name, created, last used: never the secret |
| `POST /workspaces/{id}/tokens` `{name}` | admins | 201 with the token, shown once; 403 `plan_limit` (`tokens`) |
| `DELETE /workspaces/{id}/tokens/{token}` | admins | 204 |

A product route authorizes with `Accounts::access(&headers, workspace, Action::Read)` (see
`hello` in `crates/xpay-server/src/lib.rs`). Anything the caller may not see is **404**,
so workspace ids cannot be probed; an API token reaches only its own workspace and only
reads; everything else needs a person (403 `session_required`).

## Secrets and sessions

- Invite codes (`inv_` + 160 bits), email-link tokens and session ids (256 bits) and API
  tokens (`xp_` + a 12-character display prefix + 160 bits) come from the operating system's
  random source and are stored only as SHA-256 digests. Passwords are Argon2id PHC strings.
- The session cookie is `__Host-xpay_session`: `HttpOnly`, `SameSite=Strict`, `Secure`,
  `Path=/`, no domain, so no sibling subdomain can set or shadow it. A session ends after
  `XPAY_SESSION_TTL_S` (7 days) or `XPAY_SESSION_IDLE_S` (1 day) unused; it is
  touched at most once a minute.
- Request bodies are JSON with unknown fields refused: a cross-site form cannot send
  `application/json` without a preflight, and the cookie is not sent cross-site anyway.
- Emails go through the `MailTransport` port. The template's adapter speaks plain SMTP to a
  relay on loopback (it refuses any other address: it speaks no TLS); the relay forwards to
  Postmark, Resend or SES over TLS. A product preferring a provider's HTTP API writes a
  second implementation. With no relay configured, mail is disabled and sign-up says
  `verification_email: failed`.

## Budgets

| Budget | Shape | What it bounds |
|---|---|---|
| sign-in, per address (IPv4, or IPv6 /64; behind a trusted proxy, its hop) | 5, then one per 12 s | guesses from one place; taken before the password is compared, given back on a right one |
| sign-in, global | 20, then one per 3 s | guesses spread over many addresses: taken only by attempts from a (user, address) pair that has not signed in within 30 days, right before verifying; given back on a right password |
| trusted pairs | 30 days from a success | skip the global budget: a distributed flood cannot lock the owner out of a place they use. A new device waits for the flood to pass |
| password work | 2 Argon2 jobs at once, 2 s wait | CPU; past the wait, 503 `overloaded` before anything else |
| mail, per address | 3, then one per 2 min | sign-up, resend and reset emails one client can cause |
| mail, global | 30, then one per 10 s | the same, from everywhere |

Per-address tables hold at most 16,384 addresses; a full table forgets refilled buckets
first, then the fullest, so a throttled address is never forgiven by churn.

## Plan limits

`PlanLimits` (members, tokens and owned workspaces; `XPAY_PLAN_*`) is checked at
admission, in the same transaction as the insert it bounds and under a row lock on the
workspace or the user, so concurrent requests at the edge admit exactly the limit.

## Deletion

Deleting a workspace removes its members and tokens. Deleting an account removes its
sessions, memberships, email links, trusted addresses and the workspaces it alone belonged
to; references from invites, workspaces and tokens it created are cleared. The security
log (`account_events`: kind, user id, workspace id, time; never an email, name or address)
is append-only, even for its owner, and keeps the id, which then resolves to nobody.
`xpayctl prune-events DAYS` (the owner, at least 30 days) bounds it.

## Proven by

| Test | What |
|---|---|
| `tests/flows.rs` | invite, sign-up, verification by a link from a fake SMTP sink, sign-in before and after, link reuse, invite reuse, a taken email leaving the invite unclaimed; reset ends every session; idle expiry and sign-out |
| `tests/isolation.rs` | workspace A's token, A's owner and A's member, on each of B's nine routes: 404 every time; B unchanged afterwards |
| `tests/limits.rs` and `domain::tests` | plan limits under eight concurrent requests at the edge (exactly one admitted); the property test of `admits` at its edges |
| `tests/budgets.rs` | one address's flood evaluates five guesses and touches no one else; a 40-address flood evaluates exactly 20 and the trusted owner still signs in; a right password never runs out; mail is bounded |
| `tests/deletion.rs` | after deleting an account, no kit table holds its email, and none but the security log its id; a workspace takes its rows; a sole owner with members must transfer first |
| `tests/logs.rs` | every flow at TRACE: no password, invite, link token, session id, API token or email in the log; the log still says what happened; the scan proves each needle detectable first |
| `scripts/prove-limits.sh` | the per-address, global and mail budgets, the password gate and the plan limit, each removed in turn, fail their tests |
