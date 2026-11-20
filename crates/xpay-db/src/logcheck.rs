//! The log-capture check (Backend Platform D-015, F11): captures everything a test or a
//! drill logs and fails on a known secret or a credential-shaped string.
//!
//! The check proves itself on every needle it is given: before scanning, each needle is
//! planted into a copy of the log and must be found there. A needle the scanner cannot see
//! (one that spans lines, say) fails the check instead of passing it silently (Intent
//! Studio's F11 finding). `tests/logcheck.rs` is the control: a log that does hold a
//! needle fails.

use std::io;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

use crate::testing::{OWNER_PASSWORD, RUNTIME_PASSWORD};

/// The development passwords every test environment uses (public values, but a log line
/// carrying one carries a password).
pub const DEV_SECRETS: [&str; 2] = [OWNER_PASSWORD, RUNTIME_PASSWORD];

/// Everything written, kept in memory.
#[derive(Debug, Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// The text captured so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }

    /// Appends text captured elsewhere (a child process's output).
    pub fn push(&self, text: &str) {
        let mut buffer = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        buffer.extend_from_slice(text.as_bytes());
        buffer.push(b'\n');
    }
}

/// A writer into [`Captured`].
#[derive(Debug)]
pub struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(self.0.clone())
    }
}

/// A second dispatcher registered for the life of the process. While only one dispatcher
/// is registered, tracing caches a callsite's interest from whichever thread reaches it
/// first: a parallel test with no subscriber then caches "never" for every thread, and the
/// capture silently misses that callsite. With two registered, interest is computed over
/// every live dispatcher.
static ANCHOR: OnceLock<tracing::Dispatch> = OnceLock::new();

/// Captures every `tracing` event at TRACE (sqlx's included), as JSON lines like the
/// production format, until the guard drops. Global, so events on every runtime thread are
/// caught: hold it in a test of its own binary (`tests/logs.rs`).
pub fn capture_global() -> Captured {
    ANCHOR.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_current_span(true)
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    captured
}

/// Every leak in `text`: one of `needles` or [`DEV_SECRETS`], or a URL that carries a
/// password (`scheme://user:password@host`).
pub fn leaks(text: &str, needles: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = DEV_SECRETS
        .iter()
        .chain(needles)
        .filter(|s| !s.is_empty() && text.lines().any(|line| line.contains(**s)))
        .map(|s| format!("secret {s:?}"))
        .collect();
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let after = &rest[at + 3..];
        let authority_end = after
            .find(|c: char| c == '/' || c.is_whitespace() || c == '"')
            .unwrap_or(after.len());
        let authority = &after[..authority_end];
        if let Some((userinfo, _host)) = authority.rsplit_once('@')
            && userinfo.contains(':')
        {
            found.push(format!(
                "credentials in a URL near {:?}",
                &rest[at.saturating_sub(10)..at + 3]
            ));
        }
        rest = after;
    }
    found
}

/// Panics with every leak found (the test's failure report), after proving that each
/// needle would be caught if it were there.
pub fn assert_no_leaks(text: &str, needles: &[&str], what: &str) {
    for needle in needles {
        let planted = format!("{text}\n{{\"message\":\"planted {needle} here\"}}\n");
        assert!(
            !needle.is_empty() && !leaks(&planted, &[needle]).is_empty(),
            "{what}: the scan cannot catch the needle {needle:?}, so a clean result would prove nothing"
        );
    }
    let found = leaks(text, needles);
    assert!(
        found.is_empty(),
        "{what}: secrets reached the logs: {found:?}"
    );
}
