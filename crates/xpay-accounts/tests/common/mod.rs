//! The kit in process: a fresh migrated database, the kit's routes under `/v1` behind the
//! template's middleware stack, mail through the real SMTP adapter into a fake sink, and a
//! caller that says which client address each request comes from.
#![allow(dead_code, reason = "each test binary uses part of the harness")]
#![allow(
    unreachable_pub,
    reason = "a test module shared by several test binaries"
)]
#![expect(clippy::unwrap_used, reason = "test harness")]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::get;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;
use xpay_accounts::domain::Action;
use xpay_accounts::mail::SmtpRelay;
use xpay_accounts::testing::SmtpSink;
use xpay_accounts::{Accounts, Settings, store, tokens};
use xpay_db::testing::TestDb;
use xpay_http::stack::{self, ErrorWindow, StackLimits};

pub const PASSWORD: &str = "correct horse battery staple";

pub struct Kit {
    pub db: TestDb,
    pub app: Router,
    pub sink: SmtpSink,
    pub accounts: Accounts,
}

/// What a request authenticates with.
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    None,
    Session(&'a str),
    Token(&'a str),
}

pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Answer {
    pub fn code(&self) -> Option<&str> {
        self.body["error"]["code"].as_str()
    }
}

/// A product route, as a copy would write one: readable with a token or a session.
async fn hello(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
) -> Result<axum::Json<Value>, xpay_accounts::error::AccountsError> {
    let access = accounts.access(&headers, workspace, Action::Read).await?;
    Ok(axum::Json(json!({ "workspace": access.workspace })))
}

impl Kit {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    pub async fn with(tune: impl FnOnce(&mut Settings)) -> Self {
        let db = TestDb::new().await;
        let sink = SmtpSink::start("127.0.0.1:0", None).await.unwrap();
        let mut settings = Settings {
            public_url: "https://app.example.test".to_owned(),
            cookie_secure: false,
            ..Settings::default()
        };
        tune(&mut settings);
        let accounts = Accounts::new(
            db.pool.clone(),
            settings,
            Arc::new(SmtpRelay::new(sink.addr).unwrap()),
        )
        .await
        .unwrap();
        let product = Router::new()
            .route("/workspaces/{workspace}/hello", get(hello))
            .with_state(accounts.clone());
        let app = Router::new().nest(
            "/v1",
            xpay_accounts::router(accounts.clone()).merge(product),
        );
        let app = stack::apply(
            app,
            StackLimits::default(),
            Arc::new(ErrorWindow::default()),
        );
        Self {
            db,
            app,
            sink,
            accounts,
        }
    }

    pub async fn call(
        &self,
        peer: &str,
        method: Method,
        path: &str,
        auth: Auth<'_>,
        body: Option<Value>,
    ) -> Answer {
        let mut request = Request::builder().method(method).uri(path);
        match auth {
            Auth::None => {}
            Auth::Session(id) => {
                request = request.header(header::COOKIE, format!("xpay_session={id}"));
            }
            Auth::Token(token) => {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
        }
        let body = match body {
            Some(v) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let mut request = request.body(body).unwrap();
        let peer: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Answer {
            status,
            headers,
            body,
        }
    }

    pub async fn invite(&self) -> String {
        let code = tokens::new_invite().unwrap();
        store::create_invites(
            &self.db.pool,
            &[tokens::digest(&code)],
            std::time::Duration::from_secs(3600),
        )
        .await
        .unwrap();
        code
    }

    /// Signs up through the API and verifies through the link the sink received.
    pub async fn verified_user(&self, email: &str) -> Uuid {
        let invite = self.invite().await;
        let up = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                "/v1/signup",
                Auth::None,
                Some(json!({ "invite": invite, "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
        let token = self.sink.latest_to(email).await.unwrap().token().unwrap();
        let verified = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                "/v1/verify",
                Auth::None,
                Some(json!({ "token": token })),
            )
            .await;
        assert_eq!(verified.status, StatusCode::NO_CONTENT, "{}", verified.body);
        up.body["user_id"].as_str().unwrap().parse().unwrap()
    }

    /// Signs in from `peer`; the session id.
    pub async fn session(&self, peer: &str, email: &str) -> String {
        let answer = self
            .call(
                peer,
                Method::POST,
                "/v1/session",
                Auth::None,
                Some(json!({ "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        session_from(&answer).unwrap()
    }

    pub async fn workspace(&self, session: &str, name: &str) -> Uuid {
        let answer = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                "/v1/workspaces",
                Auth::Session(session),
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
        answer.body["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn token(&self, session: &str, workspace: Uuid) -> (Uuid, String) {
        let answer = self
            .call(
                "203.0.113.1:1000",
                Method::POST,
                &format!("/v1/workspaces/{workspace}/tokens"),
                Auth::Session(session),
                Some(json!({ "name": "ci" })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
        (
            answer.body["id"].as_str().unwrap().parse().unwrap(),
            answer.body["token"].as_str().unwrap().to_owned(),
        )
    }
}

pub fn session_from(answer: &Answer) -> Option<String> {
    let cookie = answer.headers.get(header::SET_COOKIE)?.to_str().ok()?;
    let value = cookie.strip_prefix("xpay_session=")?;
    Some(value.split(';').next()?.to_owned())
}
