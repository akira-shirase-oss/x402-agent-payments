//! The paid-endpoint load scenario (`perf/scenarios/x402-paid.md`): the seller layer with the
//! in-process mock facilitator (1 ms settle latency), offered TOTAL requests at CONCURRENCY
//! in flight: fresh payments, replays, unpaid requests, clients that hang up mid-payment,
//! and a flood of fresh signers from one client address; then a phase against a full
//! store. The thresholds are in the scenario file and checked here; a miss exits non-zero
//! and the result is still written.
//!
//! ```text
//! MODE=smoke|full [CONCURRENCY=256] xpay-x402-load OUT.json
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context as _, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::routing::get;
use tokio::sync::Semaphore;
use tower::ServiceExt as _;
use xpay_facilitator_mock::{Faults, MockFacilitator};
use xpay_seller::{ClaimState, Delivery, MemoryNonceStore, NonceKey, PaymentConfig, PaymentLayer};
use xpay_x402::clock::{Clock as _, SystemClock};
use xpay_x402::{Caip2, ResourceInfo, encode_header, exact};

/// Thresholds, fixed before the first run (see the scenario file).
const MAX_P99_MS: f64 = 50.0;
/// The mock facilitator's settle latency for the whole run.
const FACILITATOR_LATENCY: Duration = Duration::from_millis(1);
/// The full-store phase: a store this small, offered this many fresh payments.
const FULL_STORE_CAPACITY: usize = 64;
const FULL_STORE_REQUESTS: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fresh,
    Replay,
    Unpaid,
    /// A fresh payment whose client hangs up right after sending it.
    HangUp,
    /// A fresh payment from a new random signer, all from one client address.
    Flood,
}

fn kind(i: usize) -> Kind {
    match i % 20 {
        0 | 10 if i >= 1000 => Kind::Replay,
        1 | 11 => Kind::Unpaid,
        3 => Kind::HangUp,
        5 => Kind::Flood,
        _ => Kind::Fresh,
    }
}

fn paid(k: Kind) -> bool {
    matches!(k, Kind::Fresh | Kind::HangUp | Kind::Flood)
}

/// Honest requests come from many addresses; the flood from one.
fn peer(i: usize, k: Kind) -> SocketAddr {
    let port = u16::try_from(i % 60_000 + 1).unwrap_or(1);
    if k == Kind::Flood {
        return SocketAddr::from(([198, 51, 100, 1], port));
    }
    let [.., a, b] = (i as u64).to_be_bytes();
    SocketAddr::from(([10, 0, a, b], port))
}

fn nonce(i: usize) -> B256 {
    let mut b = [0u8; 32];
    b[24..].copy_from_slice(&(i as u64).to_be_bytes());
    B256::from(b)
}

/// One header (and its payer) per paid request, signed on every core before the clock
/// starts. Flood requests each get a new random signer.
fn headers(
    total: usize,
    now: u64,
    offer: &xpay_x402::PaymentRequirements,
    which: fn(usize) -> Kind,
) -> anyhow::Result<Vec<Option<(String, Address)>>> {
    let signer = PrivateKeySigner::random();
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = total.div_ceil(threads);
    std::thread::scope(|s| {
        let parts: Vec<_> = (0..threads)
            .map(|t| {
                let signer = &signer;
                s.spawn(move || -> anyhow::Result<Vec<Option<(String, Address)>>> {
                    (t * chunk..((t + 1) * chunk).min(total))
                        .map(|i| {
                            let k = which(i);
                            if !paid(k) {
                                return Ok(None);
                            }
                            let fresh;
                            let key = if k == Kind::Flood {
                                fresh = PrivateKeySigner::random();
                                &fresh
                            } else {
                                signer
                            };
                            let p = exact::pay(key, key.address(), offer, None, now, nonce(i))?;
                            Ok(Some((encode_header(&p)?, key.address())))
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

/// One request through the layer, from `peer`. A hang-up drops the response future after
/// it was polled twice, then waits until the seller has ended the claim (so the seller's
/// own work stays within the offered concurrency).
async fn request(
    app: Router,
    header: Option<&str>,
    peer: SocketAddr,
    hang_up: Option<(&MemoryNonceStore, NonceKey)>,
) -> anyhow::Result<Option<u16>> {
    let mut req = Request::get("/paid");
    if let Some(h) = header {
        req = req.header("payment-signature", h);
    }
    let mut req = req.body(Body::empty())?;
    req.extensions_mut().insert(ConnectInfo(peer));
    let Some((store, key)) = hang_up else {
        return Ok(Some(app.oneshot(req).await?.status().as_u16()));
    };
    let mut fut = Box::pin(app.oneshot(req));
    let got = tokio::select! {
        biased;
        r = &mut fut => Some(r?.status().as_u16()),
        () = tokio::task::yield_now() => None,
    };
    drop(fut);
    let deadline = Instant::now() + Duration::from_secs(60);
    while matches!(store.state(&key), Some(ClaimState::InFlight)) {
        anyhow::ensure!(Instant::now() < deadline, "a hung-up claim never ended");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Ok(got)
}

/// Fresh payments against a store of [`FULL_STORE_CAPACITY`]: it serves that many, then
/// sheds with 503, and never serves a payment it could not record.
async fn full_store(now: u64, concurrency: usize) -> anyhow::Result<serde_json::Value> {
    let network = Caip2::parse(Caip2::BASE_SEPOLIA)?;
    let offer = exact::usdc_requirements(
        &network,
        alloy_primitives::U256::from(xpay_x402_dev::PRICE),
        Address::repeat_byte(0x21),
        3600,
    )
    .context("no USDC on Base Sepolia")?;
    let headers = Arc::new(headers(FULL_STORE_REQUESTS, now, &offer, |_| Kind::Fresh)?);
    let store = Arc::new(MemoryNonceStore::new(
        FULL_STORE_CAPACITY,
        Arc::new(SystemClock),
    ));
    let mut config = PaymentConfig::new(ResourceInfo::default(), vec![offer]);
    config.max_in_flight = concurrency;
    let app = Router::new()
        .route("/paid", get(|| async { "paid" }))
        .layer(PaymentLayer::new(
            config,
            Arc::new(MockFacilitator::default()),
            store.clone(),
            Arc::new(SystemClock),
        )?);
    let gate = Arc::new(Semaphore::new(concurrency));
    let mut tasks = Vec::with_capacity(FULL_STORE_REQUESTS);
    for i in 0..FULL_STORE_REQUESTS {
        let permit = gate.clone().acquire_owned().await?;
        let (app, headers) = (app.clone(), headers.clone());
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let h = headers[i].as_ref().map(|(h, _)| h.as_str());
            request(app, h, peer(i, Kind::Fresh), None).await
        }));
    }
    let (mut served, mut shed, mut unexpected) = (0usize, 0usize, 0usize);
    for t in tasks {
        match t.await?? {
            Some(200) => served += 1,
            Some(503) => shed += 1,
            _ => unexpected += 1,
        }
    }
    let pass = served > 0
        && served <= FULL_STORE_CAPACITY
        && served + shed == FULL_STORE_REQUESTS
        && unexpected == 0
        && store.len() <= FULL_STORE_CAPACITY;
    Ok(serde_json::json!({
        "capacity": FULL_STORE_CAPACITY,
        "requests": FULL_STORE_REQUESTS,
        "served": served,
        "shed": shed,
        "unexpected": unexpected,
        "pass": pass,
    }))
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
    let headers = Arc::new(headers(total, now, &load_offer()?, kind)?);
    let signing = signing.elapsed();

    let mock = MockFacilitator::default();
    mock.set_faults(Faults {
        settle_delay: Some(FACILITATOR_LATENCY),
        ..Faults::default()
    });
    let store = Arc::new(MemoryNonceStore::new(total + 1, Arc::new(SystemClock)));
    let mut config = PaymentConfig::new(
        ResourceInfo {
            url: "http://127.0.0.1:63361/paid".into(),
            ..ResourceInfo::default()
        },
        vec![load_offer()?],
    );
    // Hung-up payments finish in the seller after their client is gone.
    config.max_in_flight = concurrency * 2;
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
            store.clone(),
            Arc::new(SystemClock),
        )?);
    let asset: Address = load_offer()?.asset.parse()?;
    let key_of = move |i: usize, payer: Address| NonceKey {
        asset,
        payer,
        nonce: nonce(i),
    };

    let gate = Arc::new(Semaphore::new(concurrency));
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(total);
    for i in 0..total {
        let permit = gate.clone().acquire_owned().await?;
        let (app, headers, per_payment, store) = (
            app.clone(),
            headers.clone(),
            per_payment.clone(),
            store.clone(),
        );
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let k = kind(i);
            // i % 10 == 0, so i - 8 is a fresh one (i % 20 is 2 or 12): settled or in flight.
            let payment = if k == Kind::Replay { i - 8 } else { i };
            let signed = if k == Kind::Unpaid {
                None
            } else {
                headers[payment].as_ref()
            };
            let hang_up = match (k, signed) {
                (Kind::HangUp, Some((_, payer))) => Some((&*store, key_of(i, *payer))),
                _ => None,
            };
            let t = Instant::now();
            let status = request(app, signed.map(|(h, _)| h.as_str()), peer(i, k), hang_up).await?;
            if status == Some(200) {
                per_payment[payment].fetch_add(1, Ordering::Relaxed);
            }
            anyhow::Ok((
                k,
                status,
                u32::try_from(t.elapsed().as_micros()).unwrap_or(u32::MAX),
            ))
        }));
    }
    let mut lat = Vec::with_capacity(total);
    // A replay may overtake its original: then the replay is served and the original gets
    // 409 (in flight or settled) or 402. Correct is "each payment served exactly once",
    // counted per payment.
    let (mut ok, mut refused, mut unexpected, mut replay_served) = (0usize, 0usize, 0usize, 0usize);
    let (mut hung_up, mut flood_served, mut flood_shed) = (0usize, 0usize, 0usize);
    for t in tasks {
        let (k, status, us) = t.await??;
        if k != Kind::HangUp {
            lat.push(us);
        }
        match (k, status) {
            (Kind::Fresh, Some(200)) => ok += 1,
            (Kind::Replay, Some(200)) => replay_served += 1,
            (Kind::Fresh | Kind::Replay, Some(402 | 409)) | (Kind::Unpaid, Some(402)) => {
                refused += 1;
            }
            (Kind::HangUp, _) => hung_up += 1,
            (Kind::Flood, Some(200)) => flood_served += 1,
            (Kind::Flood, Some(503)) => flood_shed += 1,
            _ => unexpected += 1,
        }
    }
    // Every claim ended; every settlement is recorded; every 200 is a delivered claim.
    let (mut in_flight, mut settled, mut delivered, mut not_delivered) = (0, 0, 0, 0);
    for i in (0..total).filter(|i| paid(kind(*i))) {
        let Some((_, payer)) = &headers[i] else {
            continue;
        };
        match store.state(&key_of(i, *payer)) {
            Some(ClaimState::InFlight) => in_flight += 1,
            Some(ClaimState::Settled { delivery, .. }) => {
                settled += 1;
                match delivery {
                    Delivery::Delivered => delivered += 1,
                    Delivery::NotDelivered => not_delivered += 1,
                    Delivery::Serving => in_flight += 1,
                }
            }
            _ => {}
        }
    }
    let elapsed = started.elapsed();
    lat.sort_unstable();
    let pct = |per_mille: usize| {
        f64::from(lat[(lat.len() * per_mille / 1000).min(lat.len() - 1)]) / 1000.0
    };
    let fresh = (0..total).filter(|i| kind(*i) == Kind::Fresh).count();
    let full = full_store(now, concurrency).await?;
    let checks = [
        (
            "every payment served exactly once",
            (0..total)
                .filter(|i| kind(*i) == Kind::Fresh)
                .all(|i| per_payment[i].load(Ordering::Relaxed) == 1),
        ),
        ("200s == payments", ok + replay_served == fresh),
        (
            "flood payments served at most once",
            (0..total)
                .filter(|i| kind(*i) == Kind::Flood)
                .all(|i| per_payment[i].load(Ordering::Relaxed) <= 1),
        ),
        ("no claim left in flight", in_flight == 0),
        ("every settlement recorded", mock.settled_count() == settled),
        (
            "every 200 is a delivered claim",
            delivered == ok + replay_served + flood_served + per_hangup_served(&per_payment, total),
        ),
        (
            "handler runs >= 200s",
            served.load(Ordering::Relaxed) >= ok + replay_served + flood_served,
        ),
        ("no unexpected status", unexpected == 0),
        (
            "a full store serves at most its capacity, then sheds",
            full["pass"] == true,
        ),
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
        "counts": { "fresh": fresh, "served": ok, "refused": refused, "replay_served": replay_served, "unexpected": unexpected,
            "hung_up": hung_up, "flood_served": flood_served, "flood_shed": flood_shed,
            "settlements": mock.settled_count(), "handler_runs": served.load(Ordering::Relaxed),
            "claims_settled": settled, "claims_delivered": delivered, "claims_paid_not_delivered": not_delivered },
        "facilitator_latency_ms": FACILITATOR_LATENCY.as_millis(),
        "full_store": full,
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

/// Hung-up payments whose response still arrived before the hang-up.
fn per_hangup_served(per_payment: &[AtomicUsize], total: usize) -> usize {
    (0..total)
        .filter(|i| kind(*i) == Kind::HangUp)
        .map(|i| per_payment[i].load(Ordering::Relaxed))
        .sum()
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
