//! A handler slower than the wallet's own request deadline, over real loopback HTTP: the
//! wallet keeps its proof, backs off while the seller is still working, waits longer on
//! each resend, and ends with one 200 and one settlement.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alloy_primitives::U256;
use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use xpay_facilitator_mock::MockFacilitator;
use xpay_seller::{HttpFacilitator, MemoryNonceStore, PaymentConfig, PaymentLayer};
use xpay_wallet::{AllowedHost, Limits, Policy, Wallet};
use xpay_x402::clock::SystemClock;
use xpay_x402::{Caip2, ResourceInfo};
use xpay_x402_dev::{PRICE, offer, pay_to, serve, throwaway_buyer};

#[tokio::test]
async fn a_handler_slower_than_the_wallet_deadline_ends_in_one_200_and_one_settlement() {
    let mock = MockFacilitator::default();
    let fac = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fac_addr = fac.local_addr().unwrap();
    tokio::spawn(serve(fac, mock.clone().router()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = PaymentConfig::new(
        ResourceInfo {
            url: format!("http://{addr}/paid"),
            ..ResourceInfo::default()
        },
        vec![offer(Caip2::BASE_SEPOLIA).unwrap()],
    );
    config.lock_dir = None;
    let layer = PaymentLayer::new(
        config,
        Arc::new(
            HttpFacilitator::new(&format!("http://{fac_addr}/"), Duration::from_secs(10)).unwrap(),
        ),
        Arc::new(MemoryNonceStore::new(64, Arc::new(SystemClock))),
        Arc::new(SystemClock),
    )
    .unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    let runs = entered.clone();
    // The handler outlasts the wallet's deadline by a wide margin; how long either really
    // takes does not change the outcome, only how many times the wallet backs off.
    let app = Router::new()
        .route(
            "/paid",
            get(move || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1500)).await;
                "the paid content"
            }),
        )
        .layer(layer);
    tokio::spawn(serve(listener, app));

    let key = throwaway_buyer();
    let address = key.address();
    let wallet = Wallet::new(
        Arc::new(key),
        address,
        Policy {
            allow_hosts: vec![AllowedHost::new(addr.to_string(), vec![pay_to().unwrap()])],
            networks: vec![Caip2::parse(Caip2::BASE_SEPOLIA).unwrap()],
            max_per_payment: U256::from(PRICE),
            total_cap: U256::from(PRICE * 10),
            max_timeout_seconds: 300,
        },
        Limits {
            request_timeout: Duration::from_millis(300),
            ..Limits::default()
        },
        Arc::new(SystemClock),
    )
    .unwrap();
    let r = wallet.get(&format!("http://{addr}/paid")).await;
    let receipt = r.unwrap();
    assert_eq!(receipt.status, 200);
    assert_eq!(mock.settled_count(), 1, "one settlement");
    // A run the wallet gave up on is not delivered, so the same proof is served again:
    // bounded by the seller's redelivery limit, never a second payment.
    let runs = entered.load(Ordering::SeqCst);
    assert!(
        (1..=1 + usize::from(xpay_seller::store::MAX_REDELIVERIES)).contains(&runs),
        "{runs}"
    );
    assert_eq!(wallet.committed(), U256::from(PRICE), "paid once");
    assert_eq!(wallet.unresolved().len(), 0);
}
