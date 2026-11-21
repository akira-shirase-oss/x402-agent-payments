# Component manifest

Every component a copy runs or builds with: its version, where the version is pinned, who
owns keeping it current, and how a new advisory reaches that owner. "Owner" is a role in the
copy; a copy writes in its own maintainer. Maintenance is budgeted before anything is added
(the Backend Platform guide: "budget maintenance before adding adapters").

Versions as of 2026-11-17, read from this directory's lockfile, scripts and deploy files.

## Run on the droplet

| Component | Version | Pinned in | Owner | Advisory watch |
|---|---|---|---|---|
| The service (`xpay-server`, `xpayctl`) | this copy's commit | the release directory name | the copy's maintainer | the gate, daily `cargo deny` in CI |
| Caddy | 2.11.7 | `scripts/install-caddy.sh` (SHA-256 per platform), `scripts/check-caddy.sh` (image digest) | the copy's maintainer | Caddy's GitHub releases (security advisories tab) |
| Ubuntu | 24.04 LTS | the droplet image | the copy's maintainer | unattended upgrades (`deploy/harden.sh`); Ubuntu security notices |
| systemd | 255 (Ubuntu 24.04) | the distribution | the copy's maintainer | as Ubuntu |
| PostgreSQL | 18 (Neon in production; 18.7 in development and CI) | `deploy/compose.yaml` (digest); the Neon project's settings | the copy's maintainer | Neon's changelog; PostgreSQL security announcements |
| MTA relay (Postfix or similar) | the distribution's | the droplet's packages | the copy's maintainer | as Ubuntu |
| age | v1.3.2 | `scripts/install-age.sh` (SHA-256 per platform) | the copy's maintainer | the age repository's releases |
| rclone (bucket drill only) | not pinned yet | — | the copy's maintainer | rclone releases |

## Build and check

| Component | Version | Pinned in | Advisory watch |
|---|---|---|---|
| Rust | 1.99.1 | `rust-toolchain.toml`, checked by `scripts/check-toolchain.sh` | Rust security advisories; a release every six weeks |
| cargo-deny | 0.20.2 | `.github/workflows/ci.yml` (install-action, checksum, no fallback) | the RustSec database it checks, daily |
| k6 | v2.3.0 | `scripts/install-k6.sh` (SHA-256 per platform) | k6 releases |
| GitHub Actions: checkout 7.0.1, rust-cache 2.9.3, install-action 2.90.3 | commit SHAs | `.github/workflows/ci.yml`, checked by `scripts/check-pins.sh` | Dependabot (weekly, grouped minor and patch) |
| Ubuntu image for `systemd-analyze` | 24.04 (digest) | `scripts/check-unit.sh` | review when the droplet's release changes |

## Crates (direct dependencies)

All in `Cargo.toml`'s `[workspace.dependencies]`, locked in `Cargo.lock`, checked by
`cargo deny` (advisories, licenses, bans, sources) daily and proposed for update by
Dependabot weekly (grouped minor and patch; a semver-major comes alone).

| Crate | Locked | Why |
|---|---|---|
| tokio | 1.54.1 | the one async runtime |
| hyper, hyper-util | 1.12.0, 0.1.21 | the accept loop's connection bounds |
| axum | 0.8.9 | routing and extractors |
| tower, tower-http | 0.5.3, 0.7.1 | the middleware stack |
| sqlx | 0.9.0 | PostgreSQL, migrations; TLS to Neon through rustls (`tls-rustls-ring-webpki`, the bundled web PKI roots) |
| argon2 | 0.5.3 | password hashing (Argon2id) |
| sha2 | 0.10.9 | digests of every secret at rest |
| getrandom | 0.3.4 | secrets from the operating system |
| serde, serde_json | 1.0.229, 1.0.154 | JSON |
| thiserror, anyhow | 2.0.21, 1.0.104 | errors (libraries, binaries) |
| tracing, tracing-subscriber | 0.1.44, 0.3.23 | JSON logs |
| uuid, time, hex | 1.29.2, 0.3.55, 0.4.3 | ids, times, encoding |
| proptest (tests only) | 1.12.0 | the plan-limit property test; the x402 header fuzz target |
| alloy-primitives, alloy-sol-types | 1.7.3, 1.7.3 | x402 `exact`: EIP-712 hashing, signature recovery (`k256`) |
| alloy-signer, alloy-signer-local | 2.6.1, 2.6.1 | the buyer's EIP-712 signing |
| base64 | 0.22.1 | the x402 HTTP headers |
| reqwest, rustls | 0.13.5, 0.23.46 | the facilitator and buyer HTTP client, rustls on ring |
| x402 (PyPI, gate only) | 2.28.0 | the official reference for conformance, hash-pinned in `scripts/x402-reference/requirements.txt` |

`sha2` 0.11 and `getrandom` 0.4 are newer majors; Dependabot proposes each as its own pull
request, which the copy takes when it has read their changes.
