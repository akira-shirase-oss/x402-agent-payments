//! The offline x402 loop: mock facilitator, seller and buyer on loopback, every case of
//! `docs/END_GOAL.md` M1 that needs no chain. Exits non-zero on the first failure.
//!
//!   cargo run -p xpay-x402-dev --bin xpay-x402-loop

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().len() > 1 {
        anyhow::bail!("xpay-x402-loop takes no arguments");
    }
    for case in xpay_x402_dev::run_loop().await? {
        println!("ok   {:<36} {}", case.name, case.detail);
    }
    println!("x402 offline loop ok");
    Ok(())
}
