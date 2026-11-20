//! Email through one port, [`MailTransport`]. The template ships one adapter,
//! [`SmtpRelay`]: plain SMTP to a relay on loopback (Postfix or another MTA on the droplet,
//! which relays onward over TLS to Postmark, Resend or SES). It speaks no TLS itself, so it
//! refuses a relay that is not on loopback. A product that would rather call a provider's
//! HTTP API writes a second implementation of the same trait.
//!
//! Every step has a deadline and the whole send has one. Nothing here logs a message's
//! body, subject or recipient: a body carries a verification or reset link, which is a
//! secret.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;

/// One plain-text message.
#[derive(Clone)]
pub struct Message {
    /// The recipient.
    pub to: String,
    /// The subject (fixed text: never client input).
    pub subject: &'static str,
    /// The body (holds a link: a secret).
    pub body: String,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

/// Why a send failed. The text names the step, never the content.
#[derive(Debug, Error)]
pub enum MailError {
    /// The relay could not be reached or the conversation failed.
    #[error("mail relay: {0}")]
    Relay(String),
    /// A deadline passed.
    #[error("mail relay: timed out during {0}")]
    Timeout(&'static str),
    /// Mail is not configured.
    #[error("mail is not configured")]
    Disabled,
}

/// A boxed send future (the trait stays object-safe).
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<(), MailError>> + Send + 'a>>;

/// The port.
pub trait MailTransport: Send + Sync + 'static {
    /// Sends one message.
    fn send<'a>(&'a self, from: &'a str, message: &'a Message) -> SendFuture<'a>;
}

/// No mail: every send fails, so sign-up reports the verification email as `failed`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Disabled;

impl MailTransport for Disabled {
    fn send<'a>(&'a self, _from: &'a str, _message: &'a Message) -> SendFuture<'a> {
        Box::pin(async { Err(MailError::Disabled) })
    }
}

/// Plain SMTP to a relay on loopback.
#[derive(Debug, Clone)]
pub struct SmtpRelay {
    relay: SocketAddr,
    step: Duration,
    whole: Duration,
}

impl SmtpRelay {
    /// A relay at `relay`, which must be a loopback address (no TLS is spoken).
    pub fn new(relay: SocketAddr) -> Result<Self, MailError> {
        if !relay.ip().is_loopback() {
            return Err(MailError::Relay(
                "the relay must be on loopback: this adapter speaks no TLS".to_owned(),
            ));
        }
        Ok(Self {
            relay,
            step: Duration::from_secs(5),
            whole: Duration::from_secs(15),
        })
    }

    async fn converse(&self, from: &str, message: &Message) -> Result<(), MailError> {
        let step = self.step;
        let stream = tokio::time::timeout(step, TcpStream::connect(self.relay))
            .await
            .map_err(|_| MailError::Timeout("connect"))?
            .map_err(|e| MailError::Relay(format!("connect: {e}")))?;
        let mut stream = BufReader::new(stream);
        expect(&mut stream, step, "220", "greeting").await?;
        command(&mut stream, step, "EHLO localhost\r\n", "250", "EHLO").await?;
        command(
            &mut stream,
            step,
            &format!("MAIL FROM:<{from}>\r\n"),
            "250",
            "MAIL FROM",
        )
        .await?;
        command(
            &mut stream,
            step,
            &format!("RCPT TO:<{}>\r\n", message.to),
            "250",
            "RCPT TO",
        )
        .await?;
        command(&mut stream, step, "DATA\r\n", "354", "DATA").await?;
        let id = uuid::Uuid::now_v7();
        let mut data = format!(
            "From: <{from}>\r\nTo: <{to}>\r\nSubject: {subject}\r\nMessage-ID: <{id}@xpay>\r\n\
             MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n",
            to = message.to,
            subject = message.subject
        );
        for line in message.body.lines() {
            // Dot-stuffing (RFC 5321 §4.5.2).
            if line.starts_with('.') {
                data.push('.');
            }
            data.push_str(line);
            data.push_str("\r\n");
        }
        data.push_str(".\r\n");
        command(&mut stream, step, &data, "250", "message").await?;
        let _ = command(&mut stream, step, "QUIT\r\n", "221", "QUIT").await;
        Ok(())
    }
}

impl MailTransport for SmtpRelay {
    fn send<'a>(&'a self, from: &'a str, message: &'a Message) -> SendFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(self.whole, self.converse(from, message))
                .await
                .map_err(|_| MailError::Timeout("the whole send"))?
        })
    }
}

/// Longest reply line accepted.
const MAX_LINE: usize = 1024;

async fn command(
    stream: &mut BufReader<TcpStream>,
    step: Duration,
    text: &str,
    code: &str,
    what: &'static str,
) -> Result<(), MailError> {
    tokio::time::timeout(step, stream.get_mut().write_all(text.as_bytes()))
        .await
        .map_err(|_| MailError::Timeout(what))?
        .map_err(|e| MailError::Relay(format!("{what}: {e}")))?;
    expect(stream, step, code, what).await
}

/// Reads one (possibly multi-line) reply and requires its code.
async fn expect(
    stream: &mut BufReader<TcpStream>,
    step: Duration,
    code: &str,
    what: &'static str,
) -> Result<(), MailError> {
    loop {
        let mut line = String::new();
        let read = tokio::time::timeout(
            step,
            (&mut *stream).take(MAX_LINE as u64).read_line(&mut line),
        )
        .await
        .map_err(|_| MailError::Timeout(what))?
        .map_err(|e| MailError::Relay(format!("{what}: {e}")))?;
        if read == 0 {
            return Err(MailError::Relay(format!(
                "{what}: the relay closed the connection"
            )));
        }
        if !line.starts_with(code) {
            let reply: String = line.chars().take(3).collect();
            return Err(MailError::Relay(format!(
                "{what}: the relay answered {reply}"
            )));
        }
        // "250-..." continues; "250 ..." ends the reply.
        if line.as_bytes().get(3) != Some(&b'-') {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_off_loopback_is_refused() {
        assert!(SmtpRelay::new("203.0.113.5:25".parse().unwrap()).is_err());
        assert!(SmtpRelay::new("127.0.0.1:25".parse().unwrap()).is_ok());
        assert!(SmtpRelay::new("[::1]:25".parse().unwrap()).is_ok());
    }

    #[test]
    fn a_message_never_prints_its_body() {
        let m = Message {
            to: "ada@example.test".to_owned(),
            subject: "Verify",
            body: "https://x/verify?token=SECRET".to_owned(),
        };
        let shown = format!("{m:?}");
        assert!(
            !shown.contains("SECRET") && !shown.contains("ada@"),
            "{shown}"
        );
    }
}
