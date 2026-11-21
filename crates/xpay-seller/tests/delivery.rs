//! What counts as delivered, where a paid-but-not-delivered payment may be served again,
//! and who a payment slot belongs to.
#![expect(clippy::unwrap_used, reason = "test fixtures")]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use support::*;
use tower::ServiceExt as _;
use xpay_facilitator_mock::MockFacilitator;
use xpay_seller::{ClaimState, MemoryNonceStore, PaymentConfig, PaymentLayer, SettleMode};
use xpay_x402::clock::ManualClock;
use xpay_x402::{Caip2, ResourceInfo};

#[tokio::test]
async fn a_redirect_handed_over_is_delivered_and_a_retry_is_not_served_again() {
    let w = paywall(Setup {
        handler_status: StatusCode::FOUND,
        ..Setup::default()
    });
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 81));
    assert_eq!(get_paid(&w.app, Some(&h)).await.status, StatusCode::FOUND);
    let again = get_paid(&w.app, Some(&h)).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{:?}", again.body);
    assert_eq!(again.body["error"], "payment_already_settled");
    assert_eq!(w.served.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_client_error_handed_over_is_delivered() {
    let w = paywall(Setup {
        handler_status: StatusCode::NOT_FOUND,
        ..Setup::default()
    });
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 82));
    assert_eq!(
        get_paid(&w.app, Some(&h)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_paid(&w.app, Some(&h)).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn settle_after_settles_any_response_it_hands_over() {
    let w = paywall(Setup {
        mode: SettleMode::SettleAfter,
        handler_status: StatusCode::FOUND,
        ..Setup::default()
    });
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 83));
    let a = get_paid(&w.app, Some(&h)).await;
    assert_eq!(a.status, StatusCode::FOUND);
    assert!(a.headers.contains_key("payment-response"));
    assert_eq!(w.mock.settled_count(), 1);
    assert_eq!(
        get_paid(&w.app, Some(&h)).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 1);
}

/// Two routes behind one layer (one store for the payee, D-017); `/a` fails with a 5xx.
fn two_routes() -> (Router, Arc<AtomicUsize>) {
    let clock = ManualClock::at(NOW);
    let store = Arc::new(MemoryNonceStore::new(64, Arc::new(clock.clone())));
    let layer = PaymentLayer::new(
        unlocked(PaymentConfig::new(
            ResourceInfo::default(),
            vec![offer(Caip2::BASE_SEPOLIA)],
        )),
        Arc::new(MockFacilitator::new(Arc::new(clock.clone()))),
        store,
        Arc::new(clock),
    )
    .unwrap();
    let b_runs = Arc::new(AtomicUsize::new(0));
    let runs = b_runs.clone();
    let app = Router::new()
        .route("/a", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
        .route(
            "/b",
            get(move || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                "b"
            }),
        )
        .layer(layer);
    (app, b_runs)
}

async fn get_at(app: &Router, path: &str, h: &str) -> StatusCode {
    let mut req = Request::get(path)
        .header("payment-signature", h)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [203, 0, 113, 1],
            1000,
        ))));
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn a_payment_not_delivered_on_one_route_is_not_redeemed_on_another() {
    let (app, b_runs) = two_routes();
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 84));
    assert_eq!(
        get_at(&app, "/a", &h).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(get_at(&app, "/b", &h).await, StatusCode::CONFLICT);
    assert_eq!(
        b_runs.load(Ordering::SeqCst),
        0,
        "/b never ran for /a's payment"
    );
    assert_eq!(
        get_at(&app, "/a", &h).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the same route is served again"
    );
}

#[tokio::test]
async fn a_request_without_a_peer_address_is_refused_before_any_facilitator_call() {
    let w = paywall(Setup::default());
    let req = Request::get("/paid")
        .header(
            "payment-signature",
            header(&pay(&offer(Caip2::BASE_SEPOLIA), 85)),
        )
        .body(Body::empty())
        .unwrap();
    let status = w.app.clone().oneshot(req).await.unwrap().status();
    assert_ne!(status, StatusCode::OK);
    assert_eq!(w.mock.settled_count(), 0);
    assert_eq!(w.served.load(Ordering::SeqCst), 0);
    assert!(w.store.is_empty(), "nothing claimed");
}

#[tokio::test]
async fn a_settle_after_handler_that_panics_releases_the_claim() {
    let clock = ManualClock::at(NOW);
    let store = Arc::new(MemoryNonceStore::new(64, Arc::new(clock.clone())));
    let mut config = unlocked(PaymentConfig::new(
        ResourceInfo::default(),
        vec![offer(Caip2::BASE_SEPOLIA)],
    ));
    config.mode = SettleMode::SettleAfter;
    let mock = MockFacilitator::new(Arc::new(clock.clone()));
    let layer = PaymentLayer::new(
        config,
        Arc::new(mock.clone()),
        store.clone(),
        Arc::new(clock),
    )
    .unwrap();
    let app = Router::new()
        .route(
            "/a",
            get(|| async {
                assert!(!std::hint::black_box(true), "handler bug");
                "never"
            }),
        )
        .layer(layer);
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 86);
    let k = xpay_seller::NonceKey {
        asset: offer(Caip2::BASE_SEPOLIA).asset.parse().unwrap(),
        payer: buyer().address(),
        nonce: alloy_primitives::B256::repeat_byte(86),
    };
    assert_ne!(get_at(&app, "/a", &header(&p)).await, StatusCode::OK);
    let mut released = false;
    for _ in 0..10_000 {
        if store.state(&k).is_none() {
            released = true;
            break;
        }
        assert!(
            !matches!(store.state(&k), Some(ClaimState::Settled { .. })),
            "nothing may settle"
        );
        tokio::task::yield_now().await;
    }
    assert!(released, "claim kept: {:?}", store.state(&k));
    assert_eq!(mock.settled_count(), 0);
}

#[tokio::test]
async fn a_handler_that_panics_after_settlement_answers_handler_timeout_and_the_resend_is_served() {
    let clock = ManualClock::at(NOW);
    let store = Arc::new(MemoryNonceStore::new(64, Arc::new(clock.clone())));
    let mock = MockFacilitator::new(Arc::new(clock.clone()));
    let layer = PaymentLayer::new(
        unlocked(PaymentConfig::new(
            ResourceInfo::default(),
            vec![offer(Caip2::BASE_SEPOLIA)],
        )),
        Arc::new(mock.clone()),
        store.clone(),
        Arc::new(clock),
    )
    .unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let app = Router::new()
        .route(
            "/a",
            get(move || async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                assert!(n > 0, "handler bug on the first run");
                "served"
            }),
        )
        .layer(layer);
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 87));
    let k = xpay_seller::NonceKey {
        asset: offer(Caip2::BASE_SEPOLIA).asset.parse().unwrap(),
        payer: buyer().address(),
        nonce: alloy_primitives::B256::repeat_byte(87),
    };
    let mut req = Request::get("/a")
        .header("payment-signature", &h)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [203, 0, 113, 1],
            1000,
        ))));
    let first = app.clone().oneshot(req).await.unwrap();
    assert_eq!(first.status(), StatusCode::GATEWAY_TIMEOUT);
    let bytes = http_body_util::BodyExt::collect(first.into_body())
        .await
        .unwrap()
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"], xpay_seller::layer::HANDLER_TIMEOUT);
    // The fate is recorded before the answer: a resend at once finds it.
    assert!(
        matches!(
            store.state(&k),
            Some(ClaimState::Settled {
                delivery: xpay_seller::store::Delivery::NotDelivered,
                ..
            })
        ),
        "{:?}",
        store.state(&k)
    );
    assert_eq!(get_at(&app, "/a", &h).await, StatusCode::OK);
    assert_eq!(mock.settled_count(), 1, "one settlement");
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}
