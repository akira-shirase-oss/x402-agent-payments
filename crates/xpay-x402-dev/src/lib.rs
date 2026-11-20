//! The offline x402 loop over real loopback HTTP: the mock facilitator's HTTP API, a seller
//! behind [`xpay_seller::PaymentLayer`] using [`xpay_seller::HttpFacilitator`], and a
//! [`xpay_wallet::Wallet`]. No chain and no outside network: every socket is 127.0.0.1.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256, U256, address};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context as _, bail, ensure};
use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use xpay_facilitator_mock::{Faults, MockFacilitator};
use xpay_seller::{HttpFacilitator, MemoryNonceStore, PaymentConfig, PaymentLayer};
use xpay_wallet::{Limits, Policy, Wallet, WalletError};
use xpay_x402::clock::SystemClock;
use xpay_x402::exact::{self, TypedAuthorization};
use xpay_x402::{Caip2, PaymentRequirements, ResourceInfo, encode_header, reason};

/// The mock facilitator's default address (inside the port band).
pub const MOCK_FACILITATOR_ADDR: &str = "127.0.0.1:63360";
pub const SELLER: Address = address!("0x209693Bc6afc0C5328bA36FaF03C514EF312287C");
pub const PRICE: u64 = 10_000;

pub async fn serve(listener: TcpListener, app: Router) {
    // A dev binary: a failed accept loop ends the process with the error.
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server stopped: {e}");
    }
}

pub fn offer(network: &str) -> anyhow::Result<PaymentRequirements> {
    exact::usdc_requirements(&Caip2::parse(network)?, U256::from(PRICE), SELLER, 60)
        .context("no USDC on that network")
}

/// A paid `GET /paid` on `listener`, settling through the facilitator at `facilitator`.
pub fn seller(
    addr: SocketAddr,
    facilitator: &str,
    settle_timeout: Duration,
) -> anyhow::Result<Router> {
    let mut config = PaymentConfig::new(
        ResourceInfo {
            url: format!("http://{addr}/paid"),
            ..ResourceInfo::default()
        },
        vec![offer(Caip2::BASE_SEPOLIA)?],
    );
    config.settle_timeout = settle_timeout;
    let layer = PaymentLayer::new(
        config,
        Arc::new(HttpFacilitator::new(facilitator, Duration::from_secs(10))?),
        Arc::new(MemoryNonceStore::new(100_000, Arc::new(SystemClock))),
        Arc::new(SystemClock),
    );
    Ok(Router::new()
        .route("/paid", get(|| async { "the paid content" }))
        .layer(layer))
}

/// A fresh, throwaway buyer key for one run.
pub fn throwaway_buyer() -> PrivateKeySigner {
    PrivateKeySigner::random()
}

/// One line of the loop's report.
#[derive(Debug)]
pub struct Case {
    pub name: &'static str,
    pub detail: String,
}

/// Runs the loop and every failure case; `Err` names the first case that misbehaved.
#[expect(
    clippy::too_many_lines,
    reason = "one case after another, in the order the report prints them"
)]
pub async fn run_loop() -> anyhow::Result<Vec<Case>> {
    let mock = MockFacilitator::default();
    let fac_listener = TcpListener::bind("127.0.0.1:0").await?;
    let fac_addr = fac_listener.local_addr()?;
    tokio::spawn(serve(fac_listener, mock.clone().router()));
    let seller_listener = TcpListener::bind("127.0.0.1:0").await?;
    let seller_addr = seller_listener.local_addr()?;
    let app = seller(
        seller_addr,
        &format!("http://{fac_addr}/"),
        Duration::from_secs(1),
    )?;
    tokio::spawn(serve(seller_listener, app));
    let url = format!("http://{seller_addr}/paid");

    let key = throwaway_buyer();
    let address = key.address();
    let signer = Arc::new(key);
    let policy = Policy {
        allow_hosts: vec![seller_addr.to_string()],
        networks: vec![Caip2::parse(Caip2::BASE_SEPOLIA)?],
        max_per_payment: U256::from(PRICE),
        total_cap: U256::from(PRICE * 3),
        max_timeout_seconds: 300,
    };
    let wallet = Wallet::new(
        signer.clone(),
        address,
        policy.clone(),
        Limits::default(),
        Arc::new(SystemClock),
    )?;
    let mut report = Vec::new();

    // 402 -> pay -> 200.
    let r = wallet.get(&url).await?;
    let settled = r.settlement.as_ref().context("no PAYMENT-RESPONSE")?;
    ensure!(
        r.status == 200 && settled.success && r.body == b"the paid content",
        "loop: {r:?}"
    );
    ensure!(
        mock.settled_count() == 1 && wallet.committed() == U256::from(PRICE),
        "loop: books"
    );
    report.push(Case {
        name: "402 -> pay -> 200",
        detail: format!("tx {}", settled.transaction),
    });

    let raw = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let now = xpay_x402::clock::Clock::now(&SystemClock);
    let base_offer = offer(Caip2::BASE_SEPOLIA)?;
    let send = |header: String| {
        let raw = raw.clone();
        let url = url.clone();
        async move {
            let resp = raw
                .get(&url)
                .header("payment-signature", header)
                .send()
                .await?;
            let status = resp.status().as_u16();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            anyhow::Ok((
                status,
                body["error"].as_str().unwrap_or_default().to_owned(),
            ))
        }
    };
    let sign_edited = |edit: &dyn Fn(&mut TypedAuthorization),
                       req: &PaymentRequirements|
     -> anyhow::Result<String> {
        let mut p = exact::pay(signer.as_ref(), address, req, None, now, B256::random())?;
        let mut a = TypedAuthorization::parse(
            &exact::ExactEvmPayload::from_payload(&p)
                .map_err(anyhow::Error::msg)?
                .authorization,
        )
        .map_err(anyhow::Error::msg)?;
        edit(&mut a);
        p.payload = exact::sign(signer.as_ref(), &a, &base_offer)?.to_map();
        Ok(encode_header(&p)?)
    };

    let replay = sign_edited(&|_| {}, &base_offer)?;
    let first = send(replay.clone()).await?;
    let second = send(replay).await?;
    ensure!(
        first.0 == 200 && second == (402, reason::NONCE_REUSED.to_owned()),
        "replay: {first:?} {second:?}"
    );
    report.push(Case {
        name: "replayed payment",
        detail: format!("{second:?}"),
    });

    for (name, header, want) in [
        (
            "expired authorization",
            sign_edited(&|a| a.valid_before = U256::from(now - 1), &base_offer)?,
            reason::VALID_BEFORE,
        ),
        (
            "wrong amount",
            sign_edited(&|a| a.value = U256::from(PRICE - 1), &base_offer)?,
            reason::VALUE_MISMATCH,
        ),
        (
            "wrong recipient",
            sign_edited(&|a| a.to = Address::repeat_byte(0xde), &base_offer)?,
            reason::RECIPIENT_MISMATCH,
        ),
        (
            "wrong network",
            encode_header(&exact::pay(
                signer.as_ref(),
                address,
                &offer(Caip2::BASE)?,
                None,
                now,
                B256::random(),
            )?)?,
            reason::REQUIREMENTS,
        ),
        (
            "malformed header",
            "not base64 at all!".to_owned(),
            reason::PAYLOAD,
        ),
    ] {
        let got = send(header).await?;
        ensure!(
            got.1 == want && (got.0 == 402 || got.0 == 400),
            "{name}: {got:?}"
        );
        report.push(Case {
            name,
            detail: format!("{got:?}"),
        });
    }

    // The wallet refuses to pay past its caps and off its allowlist, before signing.
    let tight = Wallet::new(
        signer.clone(),
        address,
        Policy {
            max_per_payment: U256::from(PRICE - 1),
            ..policy.clone()
        },
        Limits::default(),
        Arc::new(SystemClock),
    )?;
    ensure!(
        matches!(tight.get(&url).await, Err(WalletError::OverPaymentCap)),
        "per-payment cap"
    );
    let elsewhere = Wallet::new(
        signer.clone(),
        address,
        Policy {
            allow_hosts: vec!["127.0.0.2:1".into()],
            ..policy.clone()
        },
        Limits::default(),
        Arc::new(SystemClock),
    )?;
    ensure!(
        matches!(elsewhere.get(&url).await, Err(WalletError::HostNotAllowed)),
        "allowlist"
    );
    report.push(Case {
        name: "spend cap and allowlist",
        detail: "refused before signing".into(),
    });

    // A facilitator that does not answer settle in time: the seller holds the claim and the
    // wallet keeps the amount reserved; retrying the same proof is never served.
    mock.set_faults(Faults {
        settle_delay: Some(Duration::from_secs(5)),
        ..Faults::default()
    });
    let before = wallet.committed();
    match wallet.get(&url).await {
        Err(WalletError::Unsettled(Some(504))) => {}
        other => bail!("settle timeout: {other:?}"),
    }
    ensure!(
        wallet.committed() == before + U256::from(PRICE),
        "settle timeout: reservation released"
    );
    mock.set_faults(Faults::default());
    // Total cap: 3 x price, of which 2 are committed (paid + held); one more fits, then none.
    ensure!(wallet.get(&url).await?.status == 200, "after the timeout");
    ensure!(
        matches!(wallet.get(&url).await, Err(WalletError::OverTotalCap)),
        "total cap"
    );
    report.push(Case {
        name: "facilitator timeout during settle",
        detail: "504, claim held, reservation kept; total cap then refuses".into(),
    });
    Ok(report)
}
