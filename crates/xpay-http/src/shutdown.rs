//! The signals that start a drain.

use std::io;

use tokio_util::sync::CancellationToken;

/// Installs SIGTERM and Ctrl-C handlers now (a failure is a startup error) and returns a
/// token cancelled when either arrives, and a task that does the waiting.
pub fn on_signal() -> io::Result<(CancellationToken, tokio::task::JoinHandle<&'static str>)> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let token = CancellationToken::new();
    let cancel = token.clone();
    let waiter = tokio::spawn(async move {
        let name = tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    std::future::pending::<()>().await;
                }
                "SIGINT"
            }
            _ = terminate.recv() => "SIGTERM",
        };
        cancel.cancel();
        name
    });
    Ok((token, waiter))
}
