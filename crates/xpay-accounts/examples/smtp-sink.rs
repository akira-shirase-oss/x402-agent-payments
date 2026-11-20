//! A fake SMTP sink for rehearsals: `smtp-sink <listen> <file>` accepts plain SMTP on a
//! loopback address and appends one `to=<address> token=<hex>` line per message to the
//! file (mode 0600). Never part of a shipped build.

use xpay_accounts::testing::SmtpSink;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(listen), Some(file)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: smtp-sink <listen> <file>");
    };
    let sink = SmtpSink::start(&listen, Some(file.into())).await?;
    eprintln!("smtp-sink listening on {}", sink.addr);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
