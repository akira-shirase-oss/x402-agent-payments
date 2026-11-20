# Copying the template, and keeping a copy healthy

The template is copied once and then owned. There is no shared package, service, login or
database, and nothing updates a copy automatically. Template changes reach a copy only as a
reviewed diff (below).

## A new product

From the Backend Platform repository:

```
scripts/copy-template.sh ../my-product myproduct            # copy and rename
XPAY_TEST_DATABASE_URL=... scripts/copy-template.sh ../x x --verify   # and prove the copy builds and passes
```

The script copies `template/` without its `target/`, renames everything that names the
template, and writes `TEMPLATE_ORIGIN` with the commit it came from:

| Template | Copy (`NAME` = `myproduct`) |
|---|---|
| crates `xpay-http`, `-db`, `-accounts`, `-server` | `myproduct-http`, ... |
| `xpayctl` | `myproductctl` |
| variables `XPAY_*` | `MYPRODUCT_*` |
| roles `xpay_owner`, `xpay_app` | `myproduct_owner`, `myproduct_app` |
| cookie `__Host-xpay_session` | `__Host-myproduct_session` |
| `deploy/xpay.service`, `/opt/xpay`, `/etc/xpay`, user `starter` | `myproduct.service`, `/opt/myproduct`, ... |
| compose project `xpay` | `myproduct` |

Then, by hand:

1. `git init`, push, and turn on the copied CI (`.github/workflows/ci.yml`) and Dependabot.
2. Pick the product's own ports (the template uses 63350 for the server, 63332 for the
   development database), the API-token prefix (`tokens::API_TOKEN_PREFIX`, `xp_`), and the
   `From` address.
3. Read `docs/OPERATIONS.md` and run every drill once on the product's own stores.
4. Delete what the product does not use, with what is coupled to it (below). A product
   without accounts deletes `crates/*-accounts`, `migrations/0002_accounts.sql`, the
   kit's rows in the manifest and the kit's routes in `crates/*-server/src/lib.rs`.

## An existing product

An existing product (Durable Runner, Service Monitor, Colophon) takes pieces, not the
whole workspace. Each piece lists what it brings and what it must be kept in step with.
`docs/B1T_ADOPTION.md` in the Backend Platform repository records what this took for
Durable Runner.

| Piece | Copy | Then |
|---|---|---|
| The accept loop and stack | `crates/xpay-http` (as `<name>-http`) | rewrite its `Cargo.toml` for the product's workspace (versions, `edition = "2024"`); carry `clippy.toml`'s test allowances or rewrite the tests' `expect`s; run its `tests/limits.rs` and `scripts/prove-limits.sh` there |
| PostgreSQL roles, manifest, schema gate | `crates/xpay-db`, `migrations/0001_base.sql` | the product's tables join the manifest's grants; triggers and functions join with their digests |
| The accounts kit | `crates/xpay-accounts`, `migrations/0002_accounts.sql` | mount `router()` under `/v1`; authorize product routes with `Accounts::access` |
| Host layer | `deploy/{Caddyfile,xpay.service,journald-xpay.conf,harden.sh}` | the unit's paths, credentials and `ReadWritePaths` for the product's data |
| Releases and the rehearsal | `scripts/{deploy,rollback,upgrade-rehearsal}.sh` | the migration and schema steps become the product's own compatibility check (a journal format version, say) |
| Restore drills | `scripts/drills/` | one per store the product has |
| CI and evidence kit | `scripts/check-*.sh`, `scripts/install-*.sh`, `.github/` | add the steps to the product's own gate |

## What must stay in step

Each pair below is checked by a test or script where one exists; where none does, the pair
is a review item for every change to either side.

| Change | Also change | Checked by |
|---|---|---|
| a migration that adds a table, trigger, function or grant | `crates/xpay-db/sql/protections.sql` (grants row; digest from `xpayctl digests`) | every database test (the startup check), `tests/protections.rs` |
| the PostgreSQL major | the manifest's digests (re-derive on the new major) | the same |
| a variable in `config.rs` | `deploy/env.example` | `the_environment_template_lists_every_variable_and_no_other` |
| a new bound (`// LIMIT(name)`) | a mutation row in `scripts/prove-limits.sh` and the test that proves it | `prove-limits.sh` refuses a marker without a mutation |
| `XPAY_LISTEN`'s port | `deploy/Caddyfile`'s upstream | review |
| `XPAY_DRAIN_TIMEOUT_MS` | `TimeoutStopSec=` in the unit (longer than the drain) | review |
| `XPAY_MAX_CONNECTIONS` | `LimitNOFILE=` in the unit (higher, with headroom for the pool) | review |
| the database image digest | `deploy/compose.yaml`, the root CI if it names one | `scripts/check-pins.sh` |
| the Caddy or Ubuntu image used by the checks | `scripts/check-caddy.sh`, `scripts/check-unit.sh` | `scripts/check-pins.sh` covers compose and CI only: review |
| the Rust toolchain | `rust-toolchain.toml` (CI reads it) | `scripts/check-toolchain.sh` |
| cargo-deny's version | `.github/workflows/ci.yml` (`cargo-deny@0.20.2`) | review |
| a new store | a drill under `scripts/drills/` and a row in `docs/OPERATIONS.md` | review |
| a component's version | `docs/COMPONENTS.md` | review |

## Pulling template changes

A copy never updates itself. To take a later template change:

1. Read `TEMPLATE_ORIGIN` for the commit the copy came from (`A`).
2. In the Backend Platform repository, list what changed since:
   `git log --oneline A..main -- template/` and `git diff A main -- template/`.
3. Decide per change: take it, adapt it, or record why not. Decisions in the template's
   history (`docs/DECISIONS.md` there) say why each change was made.
4. Apply the taken changes as one commit in the copy, renamed as the copy is named
   (`scripts/copy-template.sh` into a scratch directory gives the renamed text to diff
   against).
5. Run the copy's gate and every affected drill; record misses.
6. Update `template-commit` in `TEMPLATE_ORIGIN`.

The diff is reviewed like any other change: the copy owns its code, so a template change
that conflicts with the product's own is the product's decision.
