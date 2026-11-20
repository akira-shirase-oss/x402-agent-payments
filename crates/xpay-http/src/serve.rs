//! The accept loop. `axum::serve` builds hyper without a timer, so hyper's header-read
//! timeout never runs and nothing bounds what one client can hold. Here every connection is
//! served by hyper's HTTP/1 builder with a timer and these bounds (distilled from Durable
//! Runner, Accounting Reconciliation, Intent Studio and Payment Sandbox, each of which found
//! one of them missing in review):
//!
//! * **connection cap**: a permit is taken *before* `accept`, so past the cap new
//!   connections wait in the kernel's listen backlog and hold no descriptor;
//! * **header-read timeout**: a request head must arrive within it. hyper re-arms it while a
//!   keep-alive connection waits for its next request, so it is the idle timeout too;
//! * **write-stall deadline**: a response write or flush that makes no progress for it
//!   fails and closes the connection. The header timer does not run while a write is
//!   pending, so without this a client that pipelines requests and never reads the answers
//!   holds its slot for ever;
//! * **requests per connection**: the limiting response carries `Connection: close`, so
//!   pipelining cannot keep one connection busy for ever;
//! * **lifetime**: past it the connection finishes the request in flight (within
//!   `request_grace`) and closes, so slow but steady progress cannot stay for ever;
//! * **drain deadline**: shutdown stops accepting, asks every connection to finish, and
//!   waits at most `drain_timeout`.
//!
//! The request deadline and body limits are the middleware stack's ([`crate::stack`]).
//! Every connection's peer address is put in the request as
//! [`axum::extract::ConnectInfo<SocketAddr>`].

use std::future::Future as _;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Request, header};
use axum::response::Response;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Connection bounds. Every field has a configuration variable in the server
/// (`deploy/env.example`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Longest a request head may take, and longest a connection may idle between requests.
    pub header_read_timeout: Duration,
    /// Most connections served at once (keep below the unit's `LimitNOFILE`).
    pub max_connections: usize,
    /// Longest a response write may make no progress.
    pub write_stall: Duration,
    /// Most requests one connection serves.
    pub max_requests_per_connection: usize,
    /// Longest a connection lives before it is asked to finish.
    pub max_age: Duration,
    /// How long a connection past its age may take to finish its request in flight.
    pub request_grace: Duration,
    /// How long shutdown waits for open connections.
    pub drain_timeout: Duration,
}

impl Default for ConnectionLimits {
    /// Chosen, not measured: the copy's load runs set them.
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(10),
            max_connections: 512,
            write_stall: Duration::from_secs(10),
            max_requests_per_connection: 1_000,
            max_age: Duration::from_secs(600),
            request_grace: Duration::from_secs(10),
            drain_timeout: Duration::from_secs(15),
        }
    }
}

/// How serving ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// Every connection finished within the drain deadline.
    Clean,
    /// The drain deadline passed with this many connections open.
    DeadlineExceeded {
        /// Connections still open.
        open: usize,
    },
}

/// An IO that fails a write or flush making no progress for `stall`.
struct StallGuard<T> {
    inner: T,
    stall: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<T> StallGuard<T> {
    const fn new(inner: T, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            stalled: None,
        }
    }

    /// Called when a write is pending: arms the deadline, and reports once it has passed.
    fn stalled_out(&mut self, cx: &mut Context<'_>) -> Option<io::Error> {
        let stall = self.stall;
        let timer = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(stall)));
        timer
            .as_mut()
            .poll(cx)
            .is_ready()
            .then(|| io::Error::new(io::ErrorKind::TimedOut, "response write made no progress"))
    }

    fn guard<R>(
        &mut self,
        cx: &mut Context<'_>,
        polled: Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        match polled {
            Poll::Ready(result) => {
                self.stalled = None;
                Poll::Ready(result)
            }
            Poll::Pending => match self.stalled_out(cx) {
                Some(e) => Poll::Ready(Err(e)),
                None => Poll::Pending,
            },
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for StallGuard<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for StallGuard<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.guard(cx, polled)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.guard(cx, polled)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_flush(cx);
        this.guard(cx, polled)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.guard(cx, polled)
    }
}

/// Serves `app` on `listener` until `shutdown` is cancelled, then drains.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    limits: ConnectionLimits,
    shutdown: CancellationToken,
) -> Drained {
    let permits = Arc::new(Semaphore::new(limits.max_connections)); // LIMIT(connection-cap)
    let connections = TaskTracker::new();
    // Cancelled when the drain deadline passes: connections still open are cut.
    let cut = CancellationToken::new();
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new());
    http.header_read_timeout(limits.header_read_timeout); // LIMIT(header-read)
    http.keep_alive(true);
    http.max_buf_size(64 * 1024);
    loop {
        // The permit first: past the cap a connection waits in the backlog, not in a task.
        let permit = tokio::select! {
            () = shutdown.cancelled() => break,
            permit = permits.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                // Out of descriptors and the like: back off instead of spinning.
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let served = Arc::new(AtomicUsize::new(0));
        let cap = limits.max_requests_per_connection;
        let service = tower::ServiceBuilder::new()
            .map_request(move |mut request: Request<Incoming>| {
                request
                    .extensions_mut()
                    .insert(ConnectInfo::<SocketAddr>(peer));
                request
            })
            .map_response(move |mut response: Response| {
                let nth = served.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                let limiting = nth >= cap; // LIMIT(requests-per-connection)
                if limiting {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }
                response
            })
            .service(app.clone());
        let io = TokioIo::new(StallGuard::new(stream, limits.write_stall)); // LIMIT(write-stall)
        let connection = http.serve_connection(io, TowerToHyperService::new(service));
        let shutdown = shutdown.clone();
        let cut = cut.clone();
        connections.spawn(async move {
            tokio::pin!(connection);
            let aged = tokio::time::sleep(limits.max_age); // LIMIT(lifetime)
            tokio::pin!(aged);
            tokio::select! {
                result = connection.as_mut() => {
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "connection ended with an error");
                    }
                }
                () = &mut aged => {
                    connection.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(limits.request_grace, connection.as_mut()).await;
                }
                () = shutdown.cancelled() => {
                    // Finish the request in flight; the drain deadline below bounds it.
                    connection.as_mut().graceful_shutdown();
                    tokio::select! {
                        _ = connection.as_mut() => {}
                        () = cut.cancelled() => {}
                    }
                }
            }
            drop(permit);
        });
    }
    drop(listener);
    connections.close();
    let drain = limits.drain_timeout; // LIMIT(drain)
    if tokio::time::timeout(drain, connections.wait())
        .await
        .is_ok()
    {
        return Drained::Clean;
    }
    let open = connections.len();
    tracing::warn!(
        open,
        "drain deadline passed; cutting the connections still open"
    );
    cut.cancel();
    connections.wait().await;
    Drained::DeadlineExceeded { open }
}
