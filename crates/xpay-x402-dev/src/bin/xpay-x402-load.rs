//! The paid-endpoint load scenario (`perf/scenarios/x402-paid.md`): the seller layer with the
//! in-process mock facilitator, offered TOTAL requests at CONCURRENCY in flight. 80% are
//! fresh payments, 10% replays of an earlier payment, 10% unpaid. The thresholds are in the
//! scenario file and checked here; a miss exits non-zero and the result is still written.
//!
//! ```text
//! MODE=smoke|full [CONCURRENCY=256] xpay-x402-load OUT.json
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use alloy_primitives::B256;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context as _, bail};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use tokio::sync::Semaphore;
use tower::ServiceExt as _;
use xpay_facilitator_mock::MockFacilitator;
use xpay_seller::{MemoryNonceStore, PaymentConfig, PaymentLayer};
use xpay_x402::clock::{Clock as _, SystemClock};
use xpay_x402::{Caip2, ResourceInfo, encode_header, exact};

/// Thresholds, fixed before the first run (see the scenario file).
const MAX_P99_MS: f64 = 50.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fresh,
    Replay,
    Unpaid,
}

fn kind(i: usize) -> Kind {
    match i % 10 {
        0 if i >= 1000 => Kind::Replay,
        1 => Kind::Unpaid,
        _ => Kind::Fresh,
    }
}

fn nonce(i: usize) -> B256 {
    let mut b = [0u8; 32];
    b[24..].copy_from_slice(&(i as u64).to_be_bytes());
    B256::from(b)
}

/// One header per fresh request, signed on every core before the clock starts.
fn headers(total: usize, now: u64) -> anyhow::Result<Vec<Option<String>>> {
    let signer = PrivateKeySigner::random();
    let offer = load_offer()?;
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = total.div_ceil(threads);
    std::thread::scope(|s| {
        let parts: Vec<_> = (0..threads)
            .map(|t| {
                let (signer, offer) = (&signer, &offer);
                s.spawn(move || -> anyhow::Result<Vec<Option<String>>> {
                    (t * chunk..((t + 1) * chunk).min(total))
                        .map(|i| {
                            if kind(i) != Kind::Fresh {
                                return Ok(None);
                            }
                            let p =
                                exact::pay(signer, signer.address(), offer, None, now, nonce(i))?;
                            Ok(Some(encode_header(&p)?))
                        })
                        .collect()
                })
            })
            .collect();
        let mut all = Vec::with_capacity(total);
        for p in parts {
            all.extend(
                p.join()
                    .map_err(|_| anyhow::anyhow!("signer thread panicked"))??,
            );
        }
        Ok(all)
    })
}

/// The offer, valid for an hour: every request is signed before the clock starts, so the
/// authorisations must outlive the whole run (the first full run used the 60 s offer and
/// its late requests expired).
fn load_offer() -> anyhow::Result<xpay_x402::PaymentRequirements> {
    let network = Caip2::parse(Caip2::BASE_SEPOLIA)?;
    exact::usdc_requirements(
        &network,
        alloy_primitives::U256::from(xpay_x402_dev::PRICE),
        xpay_x402_dev::SELLER,
        3600,
    )
    .context("no USDC on Base Sepolia")
}

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "set up, offer, check, report: one pass"
)]
async fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .context("usage: xpay-x402-load OUT.json")?;
    let mode = std::env::var("MODE").unwrap_or_else(|_| "smoke".into());
    let total: usize = match mode.as_str() {
        "smoke" => 100_000,
        "full" => 1_000_000,
        _ => bail!("MODE must be smoke or full"),
    };
    let concurrency: usize = std::env::var("CONCURRENCY")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(256);
    let now = SystemClock.now();
    let signing = Instant::now();
    let headers = Arc::new(headers(total, now)?);
    let signing = signing.elapsed();

    let mock = MockFacilitator::default();
    let store = Arc::new(MemoryNonceStore::new(total + 1, Arc::new(SystemClock)));
    let mut config = PaymentConfig::new(
        ResourceInfo {
            url: "http://127.0.0.1:63361/paid".into(),
            ..ResourceInfo::default()
        },
        vec![load_offer()?],
    );
    config.max_in_flight = concurrency;
    let served = Arc::new(AtomicUsize::new(0));
    // Per payment (the fresh request's index): how often it was answered 200.
    let per_payment: Arc<Vec<AtomicUsize>> =
        Arc::new((0..total).map(|_| AtomicUsize::new(0)).collect());
    let counter = served.clone();
    let app: Router = Router::new()
        .route(
            "/paid",
            get(move || async move {
                counter.fetch_add(1, Ordering::Relaxed);
                "paid"
            }),
        )
        .layer(PaymentLayer::new(
            config,
            Arc::new(mock.clone()),
            store,
            Arc::new(SystemClock),
        ));

    let gate = Arc::new(Semaphore::new(concurrency));
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(total);
    for i in 0..total {
        let permit = gate.clone().acquire_owned().await?;
        let (app, headers, per_payment) = (app.clone(), headers.clone(), per_payment.clone());
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let payment = if kind(i) == Kind::Replay { i - 8 } else { i };
            let header = match kind(i) {
                Kind::Fresh => headers[i].clone(),
                // i % 10 == 0, so i - 8 is a fresh one (i % 10 == 2): settled or still in flight.
                Kind::Replay => headers[i - 8].clone(),
                Kind::Unpaid => None,
            };
            let mut req = Request::get("/paid");
            if let Some(h) = header {
                req = req.header("payment-signature", h);
            }
            let t = Instant::now();
            let status = app
                .oneshot(req.body(Body::empty())?)
                .await?
                .status()
                .as_u16();
            if status == 200 {
                per_payment[payment].fetch_add(1, Ordering::Relaxed);
            }
            anyhow::Ok((
                kind(i),
                status,
                u32::try_from(t.elapsed().as_micros()).unwrap_or(u32::MAX),
            ))
        }));
    }
    let mut lat = Vec::with_capacity(total);
    // A replay may overtake its original: then the replay is served and the original gets
    // 409 (in flight) or 402 (already used). Correct is "each payment served exactly once", counted per payment.
    let (mut ok, mut refused, mut unexpected, mut replay_served) = (0usize, 0usize, 0usize, 0usize);
    for t in tasks {
        let (k, status, us) = t.await??;
        lat.push(us);
        match (k, status) {
            (Kind::Fresh, 200) => ok += 1,
            (Kind::Replay, 200) => replay_served += 1,
            (Kind::Fresh | Kind::Replay, 402 | 409) | (Kind::Unpaid, 402) => refused += 1,
            _ => unexpected += 1,
        }
    }
    let elapsed = started.elapsed();
    lat.sort_unstable();
    let pct = |per_mille: usize| {
        f64::from(lat[(lat.len() * per_mille / 1000).min(lat.len() - 1)]) / 1000.0
    };
    let fresh = (0..total).filter(|i| kind(*i) == Kind::Fresh).count();
    let checks = [
        (
            "every payment served exactly once",
            (0..total)
                .filter(|i| kind(*i) == Kind::Fresh)
                .all(|i| per_payment[i].load(Ordering::Relaxed) == 1),
        ),
        ("200s == payments", ok + replay_served == fresh),
        (
            "handler runs == settlements == fresh payments",
            served.load(Ordering::Relaxed) == fresh && mock.settled_count() == fresh,
        ),
        ("no unexpected status", unexpected == 0),
        ("p99 below the threshold", pct(990) < MAX_P99_MS),
    ];
    let result = serde_json::json!({
        "scenario": "x402-paid",
        "mode": mode,
        "requests": total,
        "concurrency": concurrency,
        "machine": machine(),
        "commit": git(&["rev-parse", "HEAD"]),
        "dirty": !git(&["status", "--porcelain"]).is_empty(),
        "signing_seconds": signing.as_secs_f64(),
        "seconds": elapsed.as_secs_f64(),
        "requests_per_second": f64::from(u32::try_from(total).unwrap_or(u32::MAX)) / elapsed.as_secs_f64().max(f64::EPSILON),
        "latency_ms": { "p50": pct(500), "p90": pct(900), "p99": pct(990), "max": pct(1000) },
        "counts": { "fresh": fresh, "served": ok, "refused": refused, "replay_served": replay_served, "unexpected": unexpected },
        "thresholds": { "p99_ms_below": MAX_P99_MS },
        "checks": checks.iter().map(|(n, p)| serde_json::json!({ "check": n, "pass": p })).collect::<Vec<_>>(),
    });
    std::fs::write(&out, serde_json::to_vec_pretty(&result)?)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    if let Some((n, _)) = checks.iter().find(|(_, p)| !p) {
        bail!("threshold missed: {n}");
    }
    Ok(())
}

fn git(args: &[&str]) -> String {
    std::process::Command::new("git")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// CPU model, cores, memory, OS: never the host's name.
fn machine() -> serde_json::Value {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let field = |text: &str, key: &str| {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .map(|v| v.trim().to_owned())
            .unwrap_or_default()
    };
    let os = read("/etc/os-release");
    serde_json::json!({
        "cpu": field(&read("/proc/cpuinfo"), "model name"),
        "cores": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        "memory": field(&read("/proc/meminfo"), "MemTotal"),
        "os": os.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).unwrap_or_default().trim_matches('"'),
        "load_average": read("/proc/loadavg").split_whitespace().take(3).collect::<Vec<_>>().join(" "),
    })
}
