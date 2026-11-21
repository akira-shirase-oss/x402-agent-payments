//! The live Base Sepolia run, skipped with a notice unless `X402_TESTNET_KEY` is set.
//!
//! What it does when enabled: the buyer key in `X402_TESTNET_KEY` (a funded Base Sepolia
//! test wallet, hex; never committed, never logged) pays a local seller whose facilitator
//! is `X402_FACILITATOR_URL` (default the public test facilitator), for `X402_PAY_TO`
//! (default the seller address in this crate), 0.01 test USDC. Run:
//!
//! ```text
//! X402_TESTNET_KEY=... cargo test -p xpay-x402-dev --test live_base_sepolia -- --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::U256;
use alloy_signer_local::PrivateKeySigner;
use xpay_wallet::{AllowedHost, Limits, Policy, Wallet};
use xpay_x402::Caip2;
use xpay_x402::clock::SystemClock;

#[tokio::test]
async fn live_base_sepolia_pay_and_settle() {
    let Some(key) = std::env::var_os("X402_TESTNET_KEY") else {
        eprintln!(
            "SKIPPED live_base_sepolia: set X402_TESTNET_KEY (a funded Base Sepolia test key) to run it"
        );
        return;
    };
    let signer: PrivateKeySigner = key
        .to_str()
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("X402_TESTNET_KEY is not a hex private key"));
    let facilitator = std::env::var("X402_FACILITATOR_URL")
        .unwrap_or_else(|_| "https://x402.org/facilitator".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = xpay_x402_dev::seller(addr, &facilitator, Duration::from_secs(60)).unwrap();
    tokio::spawn(xpay_x402_dev::serve(listener, app));
    let address = signer.address();
    let wallet = Wallet::new(
        Arc::new(signer),
        address,
        Policy {
            allow_hosts: vec![AllowedHost::new(
                addr.to_string(),
                vec![xpay_x402_dev::pay_to().unwrap()],
            )],
            networks: vec![Caip2::parse(Caip2::BASE_SEPOLIA).unwrap()],
            max_per_payment: U256::from(xpay_x402_dev::PRICE),
            total_cap: U256::from(xpay_x402_dev::PRICE),
            max_timeout_seconds: 300,
        },
        Limits {
            request_timeout: Duration::from_secs(90),
            ..Limits::default()
        },
        Arc::new(SystemClock),
    )
    .unwrap();
    let r = wallet.get(&format!("http://{addr}/paid")).await.unwrap();
    let settled = r.settlement.unwrap();
    assert!(r.status == 200 && settled.success, "status {}", r.status);
    eprintln!(
        "live_base_sepolia: settled in transaction {}",
        settled.transaction
    );
}
