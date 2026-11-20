//! The service skeleton a product copies: configuration ([`config`]), the health contract
//! ([`health`]) and the wiring ([`app`]). The product adds its own routes next to the
//! sample one, authorizing each with [`xpay_accounts::Accounts::access`].

pub mod config;
pub mod health;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use serde_json::json;
use uuid::Uuid;
use xpay_accounts::Accounts;
use xpay_accounts::domain::Action;
use xpay_accounts::error::AccountsError;
use xpay_http::stack::{self, ErrorWindow, StackLimits};

use crate::health::Health;

/// What the router needs.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The accounts kit.
    pub accounts: Accounts,
    /// Readiness and the error window.
    pub health: Health,
    /// The error window the stack counts into.
    pub errors: Arc<ErrorWindow>,
    /// The stack's bounds.
    pub stack: StackLimits,
    /// Requests at once on `/v1` before shedding.
    pub max_in_flight: usize,
}

/// A sample product route: readable with a workspace's API token or by its members.
async fn hello(
    State(accounts): State<Accounts>,
    headers: axum::http::HeaderMap,
    Path(workspace): Path<Uuid>,
) -> Result<axum::Json<serde_json::Value>, AccountsError> {
    let access = accounts.access(&headers, workspace, Action::Read).await?;
    Ok(axum::Json(
        json!({ "workspace": access.workspace, "hello": "world" }),
    ))
}

/// The whole router with the middleware stack. `/livez`, `/readyz` and `/errorz` are
/// never shed; everything under `/v1` is.
pub fn app(state: AppState) -> Router {
    let product = Router::new()
        .route("/workspaces/{workspace}/hello", get(hello))
        .with_state(state.accounts.clone());
    let v1 = xpay_accounts::router(state.accounts.clone())
        .merge(product)
        .route_layer(axum::middleware::from_fn_with_state(
            stack::gate(state.max_in_flight),
            stack::shed,
        ));
    let health = Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .route("/errorz", get(health::errorz))
        .with_state(state.health.clone());
    let router = Router::new().nest("/v1", v1).merge(health);
    stack::apply(router, state.stack, state.errors)
}
