# Operating a copy

One service on one 2 GB droplet, behind Caddy, with its database on Neon. Every step here
has a script, and every script either checks itself or is checked by the gate. What has not
been run on a droplet says so.

**Followed end to end:** the install, check, upgrade and rollback blocks below run as
written, on every push, in CI on an Ubuntu 24.04 VM with systemd (`scripts/test-install.sh`,
job `template-install` in the Backend Platform repository): a release build, the hardening
script, the env file and credentials, the hardened unit, Caddy in front with TLS, the first
deploy, an upgrade and a rollback. The VM differs from a droplet in three ways the test
states: the database is the development PostgreSQL on 127.0.0.1 (not Neon), the domain is
`localhost` (Caddy's own CA, which the test trusts, instead of Let's Encrypt), and there is no
mail relay. **Not yet run:** on a droplet, against Neon, against a real bucket; Durable
Runner's acceptance run (Backend Platform `docs/XPAY_TEMPLATE.md` item 11).

## Install

On a fresh Ubuntu 24.04 droplet (2 GB), as root, in a checkout of the copy, with the
release binaries built (`cargo build --release --locked -p xpay-server`, here or on a
build machine; `BIN` names their directory). Set these first:

| Variable | What | Example |
|---|---|---|
| `XPAY_DOMAIN` | the name pointing at the droplet | `app.example.com` |
| `ADMIN_DATABASE_URL` | an administrator of the database: on Neon, the project owner's URL | `postgres://neondb_owner:…@ep-….neon.tech/neondb?sslmode=require` |
| `DATABASE` | the database the bootstrap creates | `app` |
| `DB_HOST` | host and port of the database, for the role URLs | `ep-….neon.tech:5432` |
| `DB_PARAMS` | URL parameters for the role URLs: Neon requires TLS | `?sslmode=require` |
| `BIN`, `RELEASE` | the binaries, and this release's name | `target/release`, the commit |

<!-- operator:install -->
```sh
# 1. Harden the droplet: SSH keys only, a firewall admitting 22, 80 and 443, unattended upgrades.
deploy/harden.sh apply

# 2. The service's user, settings and secrets. Settings hold no secret; secrets are
#    root-owned 0600 files that the unit loads as credentials.
id xpay >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin xpay
install -d -m 0755 /etc/xpay
install -d -m 0700 /etc/xpay/credentials
install -m 0644 deploy/env.example /etc/xpay/env
printf 'XPAY_PUBLIC_URL=https://%s\nXPAY_TRUSTED_PROXIES=127.0.0.1\n' "$XPAY_DOMAIN" >>/etc/xpay/env
(umask 077
  printf '%s\n' "$ADMIN_DATABASE_URL" >/etc/xpay/credentials/admin-database-url
  openssl rand -hex 24 >/etc/xpay/credentials/owner-password
  openssl rand -hex 24 >/etc/xpay/credentials/app-password)

# 3. The roles and the database (as the administrator), then the two role URLs. The
#    administrator's URL is removed afterwards: nothing that runs needs it.
XPAY_ADMIN_DATABASE_URL_FILE=/etc/xpay/credentials/admin-database-url \
  XPAY_OWNER_PASSWORD_FILE=/etc/xpay/credentials/owner-password \
  XPAY_APP_PASSWORD_FILE=/etc/xpay/credentials/app-password \
  XPAY_DATABASE_NAME="$DATABASE" "$BIN/xpayctl" bootstrap
(umask 077
  printf 'postgres://xpay_owner:%s@%s/%s%s\n' "$(cat /etc/xpay/credentials/owner-password)" \
    "$DB_HOST" "$DATABASE" "$DB_PARAMS" >/etc/xpay/credentials/migration-database-url
  printf 'postgres://xpay_app:%s@%s/%s%s\n' "$(cat /etc/xpay/credentials/app-password)" \
    "$DB_HOST" "$DATABASE" "$DB_PARAMS" >/etc/xpay/credentials/database-url)
rm /etc/xpay/credentials/admin-database-url

# 4. Bounded logs, the unit, and the first release (migrated, checked, activated, started).
install -d -m 0755 /etc/systemd/journald.conf.d
install -m 0644 deploy/journald-xpay.conf /etc/systemd/journald.conf.d/xpay.conf
systemctl restart systemd-journald
install -m 0644 deploy/xpay.service /etc/systemd/system/xpay.service
systemctl daemon-reload
systemctl enable xpay
export XPAY_MIGRATION_DATABASE_URL_FILE=/etc/xpay/credentials/migration-database-url
export XPAY_DATABASE_URL_FILE=/etc/xpay/credentials/database-url
scripts/deploy.sh --prefix /opt/xpay --release "$RELEASE" --from "$BIN" \
  --service xpay --ready-url http://127.0.0.1:63350/readyz

# 5. Caddy in front: TLS, HSTS, bounds, the upstream on loopback only.
scripts/install-caddy.sh
id caddy >/dev/null 2>&1 || useradd --system --home-dir /var/lib/caddy --shell /usr/sbin/nologin caddy
install -d -m 0755 /etc/caddy
install -m 0644 deploy/Caddyfile /etc/caddy/Caddyfile
printf 'XPAY_DOMAIN=%s\n' "$XPAY_DOMAIN" >/etc/caddy/env
install -m 0644 deploy/caddy.service /etc/systemd/system/caddy.service
systemctl daemon-reload
systemctl enable --now caddy
```
<!-- /operator:install -->

Then check it, from the droplet:

<!-- operator:check -->
```sh
deploy/harden.sh check
systemctl is-active xpay caddy
for _ in $(seq 1 60); do curl -fsS "https://$XPAY_DOMAIN/readyz" && break; sleep 1; done
curl -fsS "https://$XPAY_DOMAIN/readyz"
```
<!-- /operator:check -->

What each part does, and how it is proven:

- **Hardening** (`deploy/harden.sh`): `apply` writes an sshd drop-in, enables `ufw` and
  unattended upgrades; `check` exits 1 naming every missing step.
  `scripts/test-harden-check.sh` proves `check` passes on a hardened droplet's state and fails
  on each missing step alone; the install test runs `apply` and `check` for real.
- **Caddy** (`deploy/Caddyfile`, `deploy/caddy.service`, Caddy 2.11.7 from
  `scripts/install-caddy.sh`, checksum-verified): TLS and HSTS, header reads 10 s, bodies
  1 MB and 30 s, idle 2 min, the upstream 127.0.0.1:63350 only, the `token` query parameter
  deleted from access logs. `scripts/check-caddy.sh` validates the file; the install test
  serves through it.
- **Logs** (`deploy/journald-xpay.conf`): at most 512 MB and 30 days.
- **The unit** (`deploy/xpay.service`): `systemd-analyze security` scores it **1.4
  ("OK")** on systemd 255, recorded 2026-11-17 offline in the Ubuntu 24.04 image and natively
  in CI; `scripts/check-unit.sh` fails above 2.0. It runs as `starter` with no capabilities, a
  read-only system, no devices, a syscall filter, 768 MB at most (640 MB before reclaim
  pressure) and 4,096 descriptors, and restarts with backoff except after exits 2, 3 and 4,
  which a restart cannot fix.
- **Mail:** set `XPAY_SMTP_RELAY=127.0.0.1:25` once a local relay (Postfix as a smarthost
  to the provider) runs; until then sign-up reports `verification_email: failed`.

## Configuration and secrets

- `/etc/xpay/env`, root-owned, `0644`: every setting, from `deploy/env.example`, which
  lists each with its default and bounds. A test keeps that file and `config.rs` in step.
- `/etc/xpay/credentials/`, root-owned, files `0600`: the secrets, loaded by the unit as
  systemd credentials (`LoadCredential=`). They are never in the environment (which
  `systemctl show` prints). Outside systemd a secret comes from the file a `_FILE` variable
  names, which must be owner-only; the plain variable is for development only.
- Startup reads and checks everything before serving, and reports every problem at once
  (exit 2). The log-capture tests show no secret reaches the logs: over startup and
  shutdown (`crates/xpay-server/tests/binary.rs`) and over every accounts flow
  (`crates/xpay-accounts/tests/logs.rs`), each with a planted-leak control.

## Database roles

`xpayctl bootstrap` (an administrator: on Neon, the project's owner role) creates
`xpay_owner`, which owns everything and runs migrations, and `xpay_app`, which the
server runs as. At startup the server refuses (exit 3) to run as a superuser, as the owner,
or as a role with `CREATEROLE`, `REPLICATION` or `BYPASSRLS`, and refuses a database whose
protections differ from the manifest (`crates/xpay-db/sql/protections.sql`): every
trigger present and enabled `ALWAYS` with its exact definition, every function's exact body,
pinned search path, `SECURITY INVOKER` and owner, the runtime role's exact grants on every
table, no `TEMP`, no `CREATE`. `crates/xpay-db/tests/protections.rs` weakens a fresh
database 13 ways, each alone, and each is refused.

## Releases

```
/opt/xpay/releases/<release>/bin/{xpay-server,xpayctl}
/opt/xpay/current -> releases/<release>
/opt/xpay/previous
```

**Upgrade.** Note the restore point first: on Neon, the time (a branch can be made at it,
`scripts/drills/postgres-restore.sh --neon`); elsewhere, a `pg_dump -Fc`. Then deploy, with
the same two credentials exported as at install:

<!-- operator:upgrade -->
```sh
date -u +%Y-%m-%dT%H:%M:%SZ | tee "/root/restore-point-before-$RELEASE"
scripts/deploy.sh --prefix /opt/xpay --release "$RELEASE" --from "$BIN" \
  --service xpay --ready-url http://127.0.0.1:63350/readyz
```
<!-- /operator:upgrade -->

`deploy.sh` migrates with the new binary (as the owner, forward only), refuses to activate a
release whose schema is not `current` (exit 3), swaps `current` with one rename, restarts and
waits for readiness.

**Rollback** to the previous release:

<!-- operator:rollback -->
```sh
scripts/rollback.sh --prefix /opt/xpay --service xpay \
  --ready-url http://127.0.0.1:63350/readyz
```
<!-- /operator:rollback -->

- **Schema contract:** a binary serves only on exactly its own migrations
  (`crates/xpay-db/src/schema.rs`). A schema carrying a migration it does not know is
  `newer` and refused (exit 4); a missing one is `behind` (exit 4: migrate first); an edited
  or failed one is refused too. A newer schema is reported before the protections it also
  changes.
- So `rollback.sh` is refused (exit 4) while the database carries the newer release's
  migrations: restore the pre-upgrade snapshot first, then roll back. Writes made between the
  upgrade and that restore are lost by it. There is no expand-and-contract window yet; a copy
  that needs rollbacks without restores adds one deliberately.
- **Rehearsal:** `scripts/upgrade-rehearsal.sh` installs vN with data through the API,
  upgrades to vN+1 (this tree plus one real migration), checks the data and a token, shows vN
  and `rollback.sh` refusing the upgraded schema, restores the snapshot, rolls back and checks
  the data matches the pre-upgrade fingerprint table by table: 23 steps, each passed or failed
  with its time, in the gate. The install test runs the two blocks above on the VM (an
  upgrade with no migration, and a rollback that is allowed).

## Backup and restore

One drill per store. Each is scripted, passes only on matching row counts and checksums,
and records its steps and seconds. Off-droplet copies are encrypted with `age`; each drill
restores the key from its backup and checks it is the recipient's identity before it
decrypts, because losing the key loses every copy. `scripts/drills/test-drills.sh` proves the
drills fail on a weakened source and on a wrong key backup.

| Store | Drill | Run |
|---|---|---|
| PostgreSQL (development, or any reachable server) | `scripts/drills/postgres-restore.sh`: protections checked on the source, fingerprint, `pg_dump -Fc`, encrypt, delete the plain copy, restore the key, decrypt, restore into a fresh database, then on the copy: the protections check, the runtime role's rewrite of the security log refused, the fingerprint equal, the schema current | in the gate |
| Neon | `scripts/drills/postgres-restore.sh --neon [--promote]`: fingerprint the source, branch at that moment through the Neon API, fingerprint the branch and compare; `--promote` makes it the default branch, otherwise it is deleted. Not `pg_basebackup`, which cannot run against Neon | **not run**: no Neon credentials here |
| SQLite (Service Monitor) | `scripts/drills/sqlite-restore.sh [--db PATH]`: `VACUUM INTO`, encrypt, restore the key, decrypt into a fresh directory, `PRAGMA integrity_check`, tables compared as sorted row sets | in the gate (fixture store) |
| Append-only journal (Durable Runner) | `scripts/drills/journal-restore.sh --server --cli --handler`: jobs run to done, `/admin/backup`, pack and encrypt, restore the key, decrypt, `durable-cli restore` (checksums, replay), a second server replays: every run's id, state and finish time equal; it drains on SIGTERM | against Durable Runner's own binaries, in a scratch copy (Backend Platform `docs/B1T_ADOPTION.md`) |
| Spaces or R2 bucket (Colophon) | `scripts/drills/bucket-restore.sh --source --target`: list with hashes, export, encrypt, restore the key, decrypt, upload into an empty bucket, `rclone check` | **not run**: no bucket credentials here |

## Health

- `GET /livez`: the process answers; it never touches the database and is never shed.
- `GET /readyz`: the database probed with a deadline (`XPAY_READY_TIMEOUT_MS`, 500 ms):
  503 `unready` within it when the database is unreachable or silent, never green; one
  probe at a time, its answer reused for a second.
- `GET /errorz`: 503 once server errors over five minutes pass `XPAY_ERROR_RATE_PERMILLE`
  (5%) of at least `XPAY_ERROR_MIN_REQUESTS` (20); shed requests and timeouts count,
  health probes do not.
- Service Monitor watches `/readyz` and `/errorz` from its own droplet through the public
  URL, as a customer would; the error-rate alert is its alert on `/errorz`.

## Observability

JSON logs (one object per line) to stdout, kept by journald within its bounds. Every
request has an id (UUIDv7, or a client's if it is 1–64 safe characters), returned in
`x-request-id`, carried by every log line of the request's span and in every error body.
Logs carry ids, routes, codes and counts, never a secret, an email, a query string or a
header value. No tracing backend until a measured need exists.

## Load

The load-test standard applies to a copy's own traffic: a scenario per feature that serves
traffic, full runs of at least 1,000,000 requests on a quiet machine, results under
`perf/results/` with the machine, commit and load average and no host name
(`scripts/check-results.sh`). The template ships no scenario of its own yet: what is owed
is listed in the Backend Platform's roadmap.
