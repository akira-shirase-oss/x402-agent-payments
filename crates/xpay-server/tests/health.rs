//! The health contract in process: `/readyz` answers 503 within its deadline when the
//! database is unreachable or silent (never green, never a hang), `/livez` still answers,
//! and `/errorz` crosses its threshold.
#![expect(clippy::unwrap_used, reason = "integration test helpers")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use xpay_accounts::mail::Disabled;
use xpay_accounts::{Accounts, Settings};
use xpay_db::PoolSettings;
use xpay_http::stack::{ErrorWindow, StackLimits};
use xpay_server::health::Health;
use xpay_server::{AppState, app};

async fn app_on(database_url: &str, ready: Duration) -> (axum::Router, Arc<ErrorWindow>) {
    let settings = PoolSettings {
        acquire_timeout: Duration::from_secs(10),
        ..PoolSettings::default()
    };
    let pool = xpay_db::connect_lazy(database_url, &settings).unwrap();
    let errors = Arc::new(ErrorWindow::default());
    let accounts = Accounts::new(pool.clone(), Settings::default(), Arc::new(Disabled))
        .await
        .unwrap();
    let router = app(AppState {
        accounts,
        health: Health::new(pool, ready, errors.clone(), 50, 20),
        errors: errors.clone(),
        stack: StackLimits::default(),
        max_in_flight: 64,
    });
    (router, errors)
}

async fn get(router: &axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("203.0.113.1:1".parse::<SocketAddr>().unwrap()));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// A "database" that accepts connections and never says a word.
async fn silent_database() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    addr
}

#[tokio::test]
async fn readyz_reports_a_silent_database_within_its_deadline() {
    let addr = silent_database().await;
    let (router, _) = app_on(
        &format!("postgres://xpay_app:x@{addr}/app"),
        Duration::from_millis(300),
    )
    .await;
    let started = Instant::now();
    let (status, body) = get(&router, "/readyz").await;
    assert_eq!(
        (status, body["database"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("timeout")),
        "{body}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "answered after {:?}",
        started.elapsed()
    );
    assert_eq!(
        get(&router, "/livez").await.0,
        StatusCode::OK,
        "liveness never touches the database"
    );
}

/// A refused connection: the pool retries it until its own acquire timeout (10 s here), so
/// the probe's deadline is what answers, and still honestly.
#[tokio::test]
async fn readyz_reports_a_refused_database_within_its_deadline() {
    let (router, _) = app_on(
        "postgres://xpay_app:x@127.0.0.1:1/app",
        Duration::from_millis(300),
    )
    .await;
    let started = Instant::now();
    let (status, body) = get(&router, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        (body["status"].as_str(), body["error"]["code"].as_str()),
        (Some("unready"), Some("unready")),
        "{body}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "answered after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn errorz_alerts_only_above_the_threshold_with_enough_requests() {
    let (router, errors) = app_on(
        "postgres://xpay_app:x@127.0.0.1:1/app",
        Duration::from_millis(300),
    )
    .await;
    for _ in 0..10 {
        errors.record(StatusCode::INTERNAL_SERVER_ERROR);
    }
    assert_eq!(
        get(&router, "/errorz").await.0,
        StatusCode::OK,
        "too few requests to alert"
    );
    for _ in 0..90 {
        errors.record(StatusCode::OK);
    }
    // 10 in 100 is 100 per thousand, above 50.
    let (status, body) = get(&router, "/errorz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        (body["requests"].as_u64(), body["server_errors"].as_u64()),
        (Some(100), Some(10))
    );
    for _ in 0..200 {
        errors.record(StatusCode::OK);
    }
    assert_eq!(
        get(&router, "/errorz").await.0,
        StatusCode::OK,
        "10 in 300 is below the threshold"
    );
}
