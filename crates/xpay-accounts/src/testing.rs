//! A fake SMTP sink for tests and rehearsals: accepts plain SMTP on loopback, keeps every
//! message in memory (bounded), and can append each to a file (0600) for scripts.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;

/// Most messages kept.
const MAX_MESSAGES: usize = 1_000;
/// Largest message accepted.
const MAX_MESSAGE: usize = 64 * 1024;

/// One received message.
#[derive(Debug, Clone)]
pub struct Received {
    /// The `RCPT TO` address.
    pub to: String,
    /// Headers and body as sent (dot-stuffing undone).
    pub data: String,
}

impl Received {
    /// The first link in the message.
    pub fn link(&self) -> Option<String> {
        let at = self.data.find("http")?;
        Some(
            self.data[at..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect(),
        )
    }

    /// The first `token=` value in the message (verification and reset links).
    pub fn token(&self) -> Option<String> {
        let at = self.data.find("token=")? + "token=".len();
        Some(
            self.data[at..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect(),
        )
    }
}

/// The sink. Cheap to clone.
#[derive(Debug, Clone)]
pub struct SmtpSink {
    /// Where it listens.
    pub addr: std::net::SocketAddr,
    messages: Arc<Mutex<Vec<Received>>>,
}

impl SmtpSink {
    /// Starts a sink on `listen` (port 0 for any), appending messages to `file` if given.
    pub async fn start(listen: &str, file: Option<PathBuf>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(listen).await?;
        let addr = listener.local_addr()?;
        let messages = Arc::new(Mutex::new(Vec::new()));
        let kept = messages.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let kept = kept.clone();
                let file = file.clone();
                tokio::spawn(async move {
                    if let Some(received) = session(stream).await {
                        if let Some(path) = file {
                            append(&path, &received).await;
                        }
                        let mut all = kept.lock().unwrap_or_else(PoisonError::into_inner);
                        if all.len() < MAX_MESSAGES {
                            all.push(received);
                        }
                    }
                });
            }
        });
        Ok(Self { addr, messages })
    }

    /// Messages received so far.
    pub fn messages(&self) -> Vec<Received> {
        self.messages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The latest message to `to`, waiting up to two seconds for it.
    pub async fn latest_to(&self, to: &str) -> Option<Received> {
        for _ in 0..40 {
            if let Some(found) = self.messages().into_iter().rev().find(|m| m.to == to) {
                return Some(found);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        None
    }
}

async fn append(path: &std::path::Path, received: &Received) {
    use tokio::io::AsyncWriteExt as _;
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true).mode(0o600);
    if let Ok(mut file) = options.open(path).await {
        let line = format!(
            "to={} link={} token={}\n",
            received.to,
            received.link().unwrap_or_default(),
            received.token().unwrap_or_default()
        );
        let _ = file.write_all(line.as_bytes()).await;
    }
}

async fn session(stream: tokio::net::TcpStream) -> Option<Received> {
    let mut stream = BufReader::new(stream);
    stream
        .get_mut()
        .write_all(b"220 sink ready\r\n")
        .await
        .ok()?;
    let mut to = String::new();
    let mut received = None;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.ok()? == 0 {
            return received;
        }
        let upper = line.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            b"250 sink\r\n"
        } else if upper.starts_with("MAIL FROM") {
            b"250 ok\r\n"
        } else if upper.starts_with("RCPT TO") {
            to = line.trim()[8..]
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned();
            b"250 ok\r\n"
        } else if upper.starts_with("DATA") {
            stream.get_mut().write_all(b"354 go ahead\r\n").await.ok()?;
            let mut data = String::new();
            loop {
                let mut l = String::new();
                if stream.read_line(&mut l).await.ok()? == 0 || data.len() > MAX_MESSAGE {
                    return None;
                }
                if l == ".\r\n" {
                    break;
                }
                data.push_str(l.strip_prefix('.').unwrap_or(&l));
            }
            received = Some(Received {
                to: to.clone(),
                data,
            });
            b"250 queued\r\n"
        } else if upper.starts_with("QUIT") {
            let _ = stream.get_mut().write_all(b"221 bye\r\n").await;
            return received;
        } else {
            b"502 not implemented\r\n"
        };
        stream.get_mut().write_all(reply).await.ok()?;
    }
}
