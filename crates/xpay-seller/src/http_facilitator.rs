//! A facilitator over HTTP (`POST {base}/verify`, `POST {base}/settle`): deadlines on
//! connect and on the whole call, no redirects, a bounded answer.

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use xpay_x402::facilitator::{BoxFuture, Facilitator, FacilitatorError};
use xpay_x402::{FacilitatorRequest, SettleResponse, VerifyResponse};

/// Bound: the largest facilitator answer read.
pub const MAX_ANSWER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct HttpFacilitator {
    base: reqwest::Url,
    client: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpFacilitatorError {
    #[error("facilitator URL must be http(s) with a host")]
    Url,
    #[error("HTTP client: {0}")]
    Client(#[from] reqwest::Error),
}

impl HttpFacilitator {
    /// `base` is operator configuration (never request input). `timeout` bounds each call;
    /// the layer's own verify/settle deadlines bound it again.
    pub fn new(base: &str, timeout: Duration) -> Result<Self, HttpFacilitatorError> {
        let mut base = reqwest::Url::parse(base).map_err(|_| HttpFacilitatorError::Url)?;
        if !matches!(base.scheme(), "http" | "https") || base.host().is_none() {
            return Err(HttpFacilitatorError::Url);
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        // Another component may have installed one already; either way one is installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { base, client })
    }

    async fn call<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, FacilitatorError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| FacilitatorError::Malformed)?;
        let mut resp = self
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(transport)?;
        if !resp.status().is_success() {
            return Err(FacilitatorError::Status(resp.status().as_u16()));
        }
        let mut buf = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(transport)? {
            if buf.len() + chunk.len() > MAX_ANSWER_BYTES {
                return Err(FacilitatorError::Malformed);
            }
            buf.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&buf).map_err(|_| FacilitatorError::Malformed)
    }
}

fn transport(e: reqwest::Error) -> FacilitatorError {
    if e.is_timeout() {
        FacilitatorError::Timeout
    } else {
        FacilitatorError::Transport(e.without_url().to_string())
    }
}

impl Facilitator for HttpFacilitator {
    fn verify(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<VerifyResponse, FacilitatorError>> {
        Box::pin(async move { self.call("verify", &req).await })
    }

    fn settle(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<SettleResponse, FacilitatorError>> {
        Box::pin(async move { self.call("settle", &req).await })
    }
}
