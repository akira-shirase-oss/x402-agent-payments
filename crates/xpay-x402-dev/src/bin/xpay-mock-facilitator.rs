//! The mock x402 facilitator over HTTP, for local development: real signature checks, an
//! in-memory nonce ledger, no chain.
//!
//!   xpay-mock-facilitator [--listen 127.0.0.1:63360]

use anyhow::Context as _;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = match (args.next().as_deref(), args.next(), args.next()) {
        (None, ..) => xpay_x402_dev::MOCK_FACILITATOR_ADDR.to_owned(),
        (Some("--listen"), Some(a), None) => a,
        _ => anyhow::bail!("usage: xpay-mock-facilitator [--listen ADDR]"),
    };
    let listener = TcpListener::bind(&addr).await.context("bind")?;
    println!("listening on {}", listener.local_addr()?);
    xpay_x402_dev::serve(
        listener,
        xpay_facilitator_mock::MockFacilitator::default().router(),
    )
    .await;
    Ok(())
}
