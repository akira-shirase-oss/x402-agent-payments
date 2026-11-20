//! Shared fixtures: a paid route behind the layer, the mock facilitator, a manual clock.
#![allow(dead_code, reason = "each test binary uses a different subset")]
#![expect(clippy::unwrap_used, reason = "test fixtures")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alloy_primitives::{Address, B256, U256, address};
use alloy_signer_local::PrivateKeySigner;
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use xpay_facilitator_mock::{Faults, MockFacilitator};
use xpay_seller::{MemoryNonceStore, Paid, PaymentConfig, PaymentLayer, SettleMode};
use xpay_x402::clock::ManualClock;
use xpay_x402::{Caip2, PaymentPayload, PaymentRequirements, ResourceInfo, encode_header, exact};

pub(crate) const NOW: u64 = 1_800_000_000;
pub(crate) const SELLER: Address = address!("0x209693Bc6afc0C5328bA36FaF03C514EF312287C");

pub(crate) fn buyer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap()
}

pub(crate) fn offer(network: &str) -> PaymentRequirements {
    exact::usdc_requirements(
        &Caip2::parse(network).unwrap(),
        U256::from(10_000),
        SELLER,
        60,
    )
    .unwrap()
}

pub(crate) struct Paywall {
    pub app: Router,
    pub mock: MockFacilitator,
    pub store: Arc<MemoryNonceStore>,
    pub clock: ManualClock,
    pub served: Arc<AtomicUsize>,
}

pub(crate) struct Setup {
    pub mode: SettleMode,
    pub faults: Faults,
    pub capacity: usize,
    pub offers: Vec<PaymentRequirements>,
    pub handler_status: StatusCode,
    /// Replaces the mock facilitator in the layer (the mock still exists, unused).
    pub facilitator: Option<Arc<dyn xpay_seller::Facilitator>>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            mode: SettleMode::SettleFirst,
            faults: Faults::default(),
            capacity: 1024,
            offers: vec![offer(Caip2::BASE_SEPOLIA)],
            handler_status: StatusCode::OK,
            facilitator: None,
        }
    }
}

pub(crate) fn paywall(setup: Setup) -> Paywall {
    let clock = ManualClock::at(NOW);
    let mock = MockFacilitator::new(Arc::new(clock.clone()));
    mock.set_faults(setup.faults);
    let store = Arc::new(MemoryNonceStore::new(
        setup.capacity,
        Arc::new(clock.clone()),
    ));
    let served = Arc::new(AtomicUsize::new(0));
    let mut config = PaymentConfig::new(
        ResourceInfo {
            url: "http://127.0.0.1:63361/paid".into(),
            ..ResourceInfo::default()
        },
        setup.offers,
    );
    config.mode = setup.mode;
    config.settle_timeout = Duration::from_secs(2);
    let layer = PaymentLayer::new(
        config,
        setup
            .facilitator
            .clone()
            .unwrap_or_else(|| Arc::new(mock.clone())),
        store.clone(),
        Arc::new(clock.clone()),
    );
    let counter = served.clone();
    let status = setup.handler_status;
    let app = Router::new()
        .route(
            "/paid",
            get(move |Extension(paid): Extension<Paid>| async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (status, format!("paid by {}", paid.payer))
            }),
        )
        .layer(layer);
    Paywall {
        app,
        mock,
        store,
        clock,
        served,
    }
}

pub(crate) fn pay(req: &PaymentRequirements, nonce: u8) -> PaymentPayload {
    let b = buyer();
    exact::pay(&b, b.address(), req, None, NOW, B256::repeat_byte(nonce)).unwrap()
}

pub(crate) fn header(p: &PaymentPayload) -> String {
    encode_header(p).unwrap()
}

pub(crate) struct Answer {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: serde_json::Value,
}

pub(crate) async fn get_paid(app: &Router, payment: Option<&str>) -> Answer {
    let mut req = Request::get("/paid");
    if let Some(p) = payment {
        req = req.header("payment-signature", p);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::String(
        String::from_utf8_lossy(&bytes).into_owned(),
    ));
    Answer {
        status,
        headers,
        body,
    }
}
