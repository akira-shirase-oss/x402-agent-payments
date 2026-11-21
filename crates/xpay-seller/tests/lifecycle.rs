//! A claim always ends, facilitator work stays inside the in-flight bound, and an
//! ambiguous settlement answer never frees a nonce.
#![expect(clippy::unwrap_used, reason = "test fixtures")]

mod support;

use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_primitives::{Address, B256};
use axum::Router;
use axum::extract::Extension;
use axum::routing::get;
use support::*;
use tokio::sync::Notify;
use xpay_facilitator_mock::MockFacilitator;
use xpay_seller::{
    ClaimState, Delivery, FacilitatorError, MemoryNonceStore, NonceKey, NonceStore as _, Paid,
    PaymentConfig, PaymentLayer, SettleMode,
};
use xpay_x402::clock::ManualClock;
use xpay_x402::facilitator::BoxFuture;
use xpay_x402::{Caip2, FacilitatorRequest, ResourceInfo, SettleResponse, VerifyResponse};

fn key(nonce: u8) -> NonceKey {
    NonceKey {
        asset: Address::from_str(&offer(Caip2::BASE_SEPOLIA).asset).unwrap(),
        payer: buyer().address(),
        nonce: B256::repeat_byte(nonce),
    }
}

fn valid() -> VerifyResponse {
    VerifyResponse {
        is_valid: true,
        invalid_reason: None,
        invalid_message: None,
        payer: Some(buyer().address().to_string()),
        extensions: None,
    }
}

/// Verifies everything; `verify` can be made to hang, `settle` answers `answer`.
struct Scripted {
    hang_verify: bool,
    active_verify: Arc<AtomicUsize>,
    answer: Option<(&'static str, String)>,
}

impl xpay_seller::Facilitator for Scripted {
    fn verify(
        &self,
        _: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<VerifyResponse, FacilitatorError>> {
        Box::pin(async move {
            if self.hang_verify {
                self.active_verify.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            Ok(valid())
        })
    }

    fn settle(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<SettleResponse, FacilitatorError>> {
        Box::pin(async move {
            let (reason, transaction) = self.answer.clone().unwrap();
            Ok(SettleResponse {
                success: false,
                error_reason: Some(reason.into()),
                error_message: None,
                payer: Some(buyer().address().to_string()),
                transaction,
                network: req.payment_requirements.network,
                amount: None,
                extensions: None,
            })
        })
    }
}

struct Built {
    app: Router,
    store: Arc<MemoryNonceStore>,
    entered: Arc<AtomicUsize>,
    go: Arc<Notify>,
}

/// A paid route whose handler waits for `go` when `wait` is set.
fn build(
    mode: SettleMode,
    max_in_flight: usize,
    facilitator: Option<Arc<dyn xpay_seller::Facilitator>>,
    wait: bool,
) -> Built {
    let clock = ManualClock::at(NOW);
    let store = Arc::new(MemoryNonceStore::new(1024, Arc::new(clock.clone())));
    let mut config = unlocked(PaymentConfig::new(
        ResourceInfo {
            url: "http://127.0.0.1:63361/paid".into(),
            ..ResourceInfo::default()
        },
        vec![offer(Caip2::BASE_SEPOLIA)],
    ));
    config.mode = mode;
    config.max_in_flight = max_in_flight;
    let facilitator =
        facilitator.unwrap_or_else(|| Arc::new(MockFacilitator::new(Arc::new(clock.clone()))));
    let layer = PaymentLayer::new(config, facilitator, store.clone(), Arc::new(clock)).unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(Notify::new());
    let (e, g) = (entered.clone(), go.clone());
    let app = Router::new()
        .route(
            "/paid",
            get(move |Extension(paid): Extension<Paid>| async move {
                e.fetch_add(1, Ordering::SeqCst);
                if wait {
                    g.notified().await;
                }
                format!("paid by {}", paid.payer)
            }),
        )
        .layer(layer);
    Built {
        app,
        store,
        entered,
        go,
    }
}

async fn until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..10_000 {
        if done() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    done()
}

#[tokio::test]
async fn a_settle_after_client_that_hangs_up_does_not_strand_its_claim() {
    let b = build(SettleMode::SettleAfter, 8, None, true);
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 41));
    let app = b.app.clone();
    let req = tokio::spawn(async move { get_paid(&app, Some(&h)).await.status });
    assert!(until(|| b.entered.load(Ordering::SeqCst) == 1).await);
    req.abort();
    let _ = req.await;
    b.go.notify_one();
    let store = b.store.clone();
    let ended = until(|| !matches!(store.state(&key(41)), Some(ClaimState::InFlight))).await;
    assert!(ended, "claim still {:?}", b.store.state(&key(41)));
    assert!(
        matches!(b.store.state(&key(41)), Some(ClaimState::Settled { .. })),
        "{:?}",
        b.store.state(&key(41))
    );
}

#[tokio::test]
async fn hang_ups_cannot_grow_facilitator_work_past_the_in_flight_bound() {
    let active = Arc::new(AtomicUsize::new(0));
    let f = Arc::new(Scripted {
        hang_verify: true,
        active_verify: active.clone(),
        answer: None,
    });
    let b = build(SettleMode::SettleFirst, 2, Some(f), false);
    for i in 0..10u8 {
        let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 100 + i));
        let app = b.app.clone();
        let req = tokio::spawn(async move { get_paid(&app, Some(&h)).await.status });
        let before = active.load(Ordering::SeqCst);
        until(|| req.is_finished() || active.load(Ordering::SeqCst) > before).await;
        req.abort();
        let _ = req.await;
    }
    assert!(
        active.load(Ordering::SeqCst) <= 2,
        "{} facilitator calls running with a bound of 2",
        active.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn an_unexpected_settle_error_holds_the_claim() {
    for (reason, transaction) in [
        ("unexpected_settle_error", String::new()),
        ("invalid_exact_evm_nonce_already_used", String::new()),
        ("insufficient_funds", format!("0x{}", "cd".repeat(32))),
    ] {
        let f = Arc::new(Scripted {
            hang_verify: false,
            active_verify: Arc::new(AtomicUsize::new(0)),
            answer: Some((reason, transaction)),
        });
        let b = build(SettleMode::SettleFirst, 8, Some(f), false);
        let a = get_paid(&b.app, Some(&header(&pay(&offer(Caip2::BASE_SEPOLIA), 51)))).await;
        assert_ne!(a.status, axum::http::StatusCode::OK, "{reason}");
        assert!(
            matches!(b.store.state(&key(51)), Some(ClaimState::Held { .. })),
            "{reason}: {:?}",
            b.store.state(&key(51))
        );
        assert_eq!(b.entered.load(Ordering::SeqCst), 0, "{reason}");
    }
}

#[tokio::test]
async fn a_pre_broadcast_settle_refusal_still_releases() {
    let f = Arc::new(Scripted {
        hang_verify: false,
        active_verify: Arc::new(AtomicUsize::new(0)),
        answer: Some(("insufficient_funds", String::new())),
    });
    let b = build(SettleMode::SettleFirst, 8, Some(f), false);
    let a = get_paid(&b.app, Some(&header(&pay(&offer(Caip2::BASE_SEPOLIA), 52)))).await;
    assert_eq!(a.status, axum::http::StatusCode::PAYMENT_REQUIRED);
    assert_eq!(b.store.state(&key(52)), None);
}

#[tokio::test]
async fn a_paid_payment_whose_client_hung_up_is_served_on_retry_once() {
    let b = build(SettleMode::SettleFirst, 8, None, true);
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 61));
    let (app, h1) = (b.app.clone(), h.clone());
    let req = tokio::spawn(async move { get_paid(&app, Some(&h1)).await.status });
    assert!(until(|| b.entered.load(Ordering::SeqCst) == 1).await);
    req.abort();
    let _ = req.await;
    b.go.notify_one();
    let store = b.store.clone();
    assert!(
        until(|| matches!(
            store.state(&key(61)),
            Some(ClaimState::Settled {
                delivery: Delivery::NotDelivered,
                ..
            })
        ))
        .await,
        "{:?}",
        b.store.state(&key(61))
    );
    assert_eq!(
        b.store.held().await,
        vec![(key(61), xpay_seller::store::PAID_NOT_DELIVERED)]
    );
    b.go.notify_one();
    let again = get_paid(&b.app, Some(&h)).await;
    assert_eq!(again.status, axum::http::StatusCode::OK, "{:?}", again.body);
    assert!(again.headers.contains_key("payment-response"));
    assert_eq!(b.entered.load(Ordering::SeqCst), 2);
    assert_eq!(b.store.held().await.len(), 0);
    let third = get_paid(&b.app, Some(&h)).await;
    assert_eq!(third.status, axum::http::StatusCode::CONFLICT);
    assert_eq!(b.entered.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn one_client_cannot_take_every_payment_slot() {
    let active = Arc::new(AtomicUsize::new(0));
    let f = Arc::new(Scripted {
        hang_verify: true,
        active_verify: active.clone(),
        answer: None,
    });
    let clock = ManualClock::at(NOW);
    let store = Arc::new(MemoryNonceStore::new(64, Arc::new(clock.clone())));
    let mut config = unlocked(PaymentConfig::new(
        ResourceInfo::default(),
        vec![offer(Caip2::BASE_SEPOLIA)],
    ));
    config.max_in_flight_per_client = 2;
    let layer = PaymentLayer::new(config, f, store, Arc::new(clock)).unwrap();
    let app = Router::new()
        .route("/paid", get(|| async { "ok" }))
        .layer(layer);
    let from = |ip: &str, n: u8| {
        let peer: std::net::SocketAddr = format!("{ip}:1000").parse().unwrap();
        let app = app.clone();
        let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), n));
        async move {
            let mut r = axum::http::Request::get("/paid")
                .header("payment-signature", h)
                .body(axum::body::Body::empty())
                .unwrap();
            r.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            tower::ServiceExt::oneshot(app, r).await.unwrap().status()
        }
    };
    let mut pending = Vec::new();
    for n in 0..2 {
        pending.push(tokio::spawn(from("198.51.100.7", 70 + n)));
    }
    assert!(until(|| active.load(Ordering::SeqCst) == 2).await);
    assert_eq!(
        from("198.51.100.7", 72).await,
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let other = tokio::spawn(from("203.0.113.9", 73));
    assert!(
        until(|| active.load(Ordering::SeqCst) == 3).await,
        "another client still gets a slot"
    );
    other.abort();
    for p in pending {
        p.abort();
    }
}

#[tokio::test]
async fn a_payee_gets_one_store_per_process() {
    let clock = ManualClock::at(NOW);
    let mk = |store: Arc<MemoryNonceStore>| {
        PaymentLayer::new(
            unlocked(PaymentConfig::new(
                ResourceInfo::default(),
                vec![offer(Caip2::BASE_SEPOLIA)],
            )),
            Arc::new(MockFacilitator::new(Arc::new(clock.clone()))),
            store,
            Arc::new(clock.clone()),
        )
    };
    let store = Arc::new(MemoryNonceStore::new(8, Arc::new(clock.clone())));
    let first = mk(store.clone()).unwrap();
    let same_store = mk(store.clone()).unwrap();
    let other = Arc::new(MemoryNonceStore::new(8, Arc::new(clock.clone())));
    assert_eq!(
        mk(other.clone()).unwrap_err(),
        xpay_seller::ConfigError::PayToTaken(0)
    );
    drop((first, same_store));
    assert!(mk(other).is_ok(), "free again once the first layer is gone");
}

#[tokio::test]
async fn a_second_process_for_a_payee_is_refused_by_the_lock() {
    let dir = std::env::temp_dir().join(format!("xpay-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let clock = ManualClock::at(NOW);
    let mut config = PaymentConfig::new(ResourceInfo::default(), vec![offer(Caip2::BASE_SEPOLIA)]);
    config.lock_dir = Some(dir.clone());
    let store = Arc::new(MemoryNonceStore::new(8, Arc::new(clock.clone())));
    let _held = PaymentLayer::new(
        config.clone(),
        Arc::new(MockFacilitator::new(Arc::new(clock.clone()))),
        store.clone(),
        Arc::new(clock.clone()),
    )
    .unwrap();
    // A second open of the same lock file stands in for another process.
    let entry = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
    let f = std::fs::File::open(entry.path()).unwrap();
    assert!(f.try_lock().is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn offers_are_checked_against_the_usdc_table() {
    let clock = ManualClock::at(NOW);
    let mut bad = offer(Caip2::BASE_SEPOLIA);
    bad.asset = "0x0000000000000000000000000000000000000001".into();
    let mut zero = offer(Caip2::BASE_SEPOLIA);
    zero.amount = "0".into();
    for (o, want) in [(bad, 0), (zero, 0)] {
        let r = PaymentLayer::new(
            unlocked(PaymentConfig::new(ResourceInfo::default(), vec![o])),
            Arc::new(MockFacilitator::new(Arc::new(clock.clone()))),
            Arc::new(MemoryNonceStore::new(8, Arc::new(clock.clone()))),
            Arc::new(clock.clone()),
        );
        assert_eq!(r.unwrap_err(), xpay_seller::ConfigError::Offer(want));
    }
}

#[tokio::test]
async fn clients_behind_a_proxy_chain_are_told_apart() {
    let active = Arc::new(AtomicUsize::new(0));
    let f = Arc::new(Scripted {
        hang_verify: true,
        active_verify: active.clone(),
        answer: None,
    });
    let clock = ManualClock::at(NOW);
    let store = Arc::new(MemoryNonceStore::new(64, Arc::new(clock.clone())));
    let mut config = unlocked(PaymentConfig::new(
        ResourceInfo::default(),
        vec![offer(Caip2::BASE_SEPOLIA)],
    ));
    config.max_in_flight_per_client = 1;
    // Caddy on loopback, then a CDN egress range in front of it.
    config.trusted_proxies = vec![
        "127.0.0.1".parse().unwrap(),
        "198.51.100.0/24".parse().unwrap(),
    ];
    let layer = PaymentLayer::new(config, f, store, Arc::new(clock)).unwrap();
    let app = Router::new()
        .route("/paid", get(|| async { "ok" }))
        .layer(layer);
    let via_cdn = |client: &str, n: u8| {
        let app = app.clone();
        let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), n));
        let xff = format!("{client}, 198.51.100.20");
        async move {
            let mut r = axum::http::Request::get("/paid")
                .header("payment-signature", h)
                .header("x-forwarded-for", xff)
                .body(axum::body::Body::empty())
                .unwrap();
            r.extensions_mut().insert(axum::extract::ConnectInfo(
                "127.0.0.1:1000".parse::<std::net::SocketAddr>().unwrap(),
            ));
            tower::ServiceExt::oneshot(app, r).await.unwrap().status()
        }
    };
    let first = tokio::spawn(via_cdn("203.0.113.5", 90));
    assert!(until(|| active.load(Ordering::SeqCst) == 1).await);
    assert_eq!(
        via_cdn("203.0.113.5", 91).await,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "the same client is still limited"
    );
    let other = tokio::spawn(via_cdn("203.0.113.6", 92));
    assert!(
        until(|| active.load(Ordering::SeqCst) == 2).await,
        "another client behind the same CDN egress gets its own slot"
    );
    first.abort();
    other.abort();
}

#[tokio::test]
async fn the_payee_lock_is_on_by_default_and_one_store_may_serve_many_routes() {
    let clock = ManualClock::at(NOW);
    let config = PaymentConfig::new(ResourceInfo::default(), vec![offer(Caip2::BASE_SEPOLIA)]);
    assert!(
        config.lock_dir.is_some(),
        "the per-host lock is on by default"
    );
    let store = Arc::new(MemoryNonceStore::new(8, Arc::new(clock.clone())));
    let mk = || {
        PaymentLayer::new(
            config.clone(),
            Arc::new(MockFacilitator::new(Arc::new(clock.clone()))),
            store.clone(),
            Arc::new(clock.clone()),
        )
    };
    let first = mk().unwrap();
    let second = mk().expect("the same store on a second route takes the lock once");
    drop((first, second));
}

#[tokio::test]
async fn one_network_cannot_take_every_payment_slot() {
    let active = Arc::new(AtomicUsize::new(0));
    let f = Arc::new(Scripted {
        hang_verify: true,
        active_verify: active.clone(),
        answer: None,
    });
    let clock = ManualClock::at(NOW);
    let store = Arc::new(MemoryNonceStore::new(64, Arc::new(clock.clone())));
    let mut config = unlocked(PaymentConfig::new(
        ResourceInfo::default(),
        vec![offer(Caip2::BASE_SEPOLIA)],
    ));
    config.max_in_flight_per_client = 1;
    config.max_in_flight_per_network = 2;
    let layer = PaymentLayer::new(config, f, store, Arc::new(clock)).unwrap();
    let app = Router::new()
        .route("/paid", get(|| async { "ok" }))
        .layer(layer);
    // One /48, a fresh /64 per request: rotating addresses is not a new client.
    let from = |peer: &str, n: u8| {
        let app = app.clone();
        let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), n));
        let peer: std::net::SocketAddr = format!("[{peer}]:1000").parse().unwrap();
        async move {
            let mut r = axum::http::Request::get("/paid")
                .header("payment-signature", h)
                .body(axum::body::Body::empty())
                .unwrap();
            r.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            tower::ServiceExt::oneshot(app, r).await.unwrap().status()
        }
    };
    let a = tokio::spawn(from("2001:db8:1:1::1", 100));
    let b = tokio::spawn(from("2001:db8:1:2::1", 101));
    assert!(until(|| active.load(Ordering::SeqCst) == 2).await);
    assert_eq!(
        from("2001:db8:1:3::1", 102).await,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "a third /64 in the same /48 waits for the network's slots"
    );
    let other = tokio::spawn(from("2001:db8:2:1::1", 103));
    assert!(
        until(|| active.load(Ordering::SeqCst) == 3).await,
        "another network is not affected"
    );
    a.abort();
    b.abort();
    other.abort();
}
