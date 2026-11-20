//! The kit's routes, mounted by the product under `/v1`. Request bodies are JSON (a
//! cross-site form cannot send `application/json` without a preflight), unknown fields
//! are refused, and every answer to an email address says nothing about whether an account
//! exists, except sign-up's `email_taken` (sign-up is invite-gated).

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use xpay_http::client::ClientKey;

use crate::domain::{self, Action, PasswordProblem, Role};
use crate::error::AccountsError;
use crate::signin::Attempt;
use crate::{Accounts, Via, store, tokens};

/// The kit's router, with its state; nest it under `/v1`.
pub fn router(accounts: Accounts) -> Router {
    Router::new()
        .route("/signup", post(sign_up))
        .route("/verify", post(verify))
        .route("/verify/resend", post(resend))
        .route("/password-reset", post(request_reset))
        .route("/password-reset/confirm", post(confirm_reset))
        .route("/session", post(sign_in).delete(sign_out))
        .route("/me", get(me).delete(delete_me))
        .route("/workspaces", post(create_workspace))
        .route(
            "/workspaces/{workspace}",
            get(read_workspace).delete(delete_workspace),
        )
        .route(
            "/workspaces/{workspace}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/workspaces/{workspace}/members/{user}",
            delete(remove_member),
        )
        .route(
            "/workspaces/{workspace}/tokens",
            get(list_tokens).post(create_token),
        )
        .route(
            "/workspaces/{workspace}/tokens/{token}",
            delete(delete_token),
        )
        .with_state(accounts)
}

type Result<T> = std::result::Result<T, AccountsError>;

fn client(accounts: &Accounts, peer: SocketAddr, headers: &HeaderMap) -> ClientKey {
    ClientKey::from_request(peer, headers, &accounts.settings().trusted_proxies)
}

fn email(raw: &str) -> Result<String> {
    domain::normalize_email(raw).ok_or(AccountsError::Invalid(
        "invalid_email",
        "the email address is not valid",
    ))
}

fn new_password(password: &str) -> Result<()> {
    domain::check_password(password).map_err(|p| match p {
        PasswordProblem::TooShort => {
            AccountsError::Invalid("weak_password", "a password needs at least 12 characters")
        }
        PasswordProblem::TooLong => {
            AccountsError::Invalid("password_too_long", "a password may have at most 512 bytes")
        }
    })
}

fn internal(e: impl Into<anyhow::Error>) -> AccountsError {
    AccountsError::Internal(e.into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignUp {
    invite: String,
    email: String,
    password: String,
}

async fn sign_up(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<SignUp>,
) -> Result<Response> {
    accounts
        .mail_gate()
        .admit(client(&accounts, peer, &headers))
        .map_err(|w| AccountsError::RateLimited(w.secs()))?;
    let email = email(&body.email)?;
    new_password(&body.password)?;
    if body.invite.len() > 64 {
        return Err(AccountsError::InviteInvalid);
    }
    let hash = accounts.passwords().hash(body.password).await?;
    let verify = tokens::new_hex_secret().map_err(internal)?;
    let id = Uuid::now_v7();
    store::sign_up(
        accounts.db(),
        &store::NewUser {
            invite: tokens::digest(body.invite.trim()),
            id,
            email: &email,
            password_hash: &hash,
            verify: tokens::digest(&verify),
            verify_ttl: accounts.settings().verify_ttl,
        },
    )
    .await?;
    tracing::info!(user = %id, "account created");
    let sent = send_verification(&accounts, &email, &verify).await;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user_id": id, "verification_email": if sent { "sent" } else { "failed" } })),
    )
        .into_response())
}

async fn send_verification(accounts: &Accounts, email: &str, token: &str) -> bool {
    let link = format!("{}/verify?token={token}", accounts.settings().public_url);
    accounts
        .send(
            email,
            "Verify your email",
            format!(
                "Open this link to verify your email:\n\n{link}\n\nIt works once, for 24 hours.\n"
            ),
        )
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBody {
    token: String,
}

async fn verify(
    State(accounts): State<Accounts>,
    Json(body): Json<TokenBody>,
) -> Result<StatusCode> {
    if !tokens::looks_like_hex_secret(&body.token) {
        return Err(AccountsError::TokenInvalid);
    }
    if store::verify(accounts.db(), tokens::digest(&body.token)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AccountsError::TokenInvalid)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmailBody {
    email: String,
}

/// 202 whatever the address: an unverified account gets a fresh link.
async fn resend(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<EmailBody>,
) -> Result<StatusCode> {
    accounts
        .mail_gate()
        .admit(client(&accounts, peer, &headers))
        .map_err(|w| AccountsError::RateLimited(w.secs()))?;
    let email = email(&body.email)?;
    if let Some(user) = store::user_by_email(accounts.db(), &email).await?
        && !user.verified
    {
        let token = tokens::new_hex_secret().map_err(internal)?;
        store::replace_email_token(
            accounts.db(),
            user.id,
            "verify",
            tokens::digest(&token),
            accounts.settings().verify_ttl,
        )
        .await?;
        send_verification(&accounts, &email, &token).await;
    }
    Ok(StatusCode::ACCEPTED)
}

/// 202 whatever the address: an existing account gets a reset link.
async fn request_reset(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<EmailBody>,
) -> Result<StatusCode> {
    accounts
        .mail_gate()
        .admit(client(&accounts, peer, &headers))
        .map_err(|w| AccountsError::RateLimited(w.secs()))?;
    let email = email(&body.email)?;
    if let Some(user) = store::user_by_email(accounts.db(), &email).await? {
        let token = tokens::new_hex_secret().map_err(internal)?;
        store::replace_email_token(
            accounts.db(),
            user.id,
            "reset",
            tokens::digest(&token),
            accounts.settings().reset_ttl,
        )
        .await?;
        let link = format!("{}/reset?token={token}", accounts.settings().public_url);
        accounts
            .send(
                &email,
                "Reset your password",
                format!("Open this link to choose a new password:\n\n{link}\n\nIt works once, for one hour. If you did not ask for it, ignore this email.\n"),
            )
            .await;
    }
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmReset {
    token: String,
    password: String,
}

async fn confirm_reset(
    State(accounts): State<Accounts>,
    Json(body): Json<ConfirmReset>,
) -> Result<StatusCode> {
    if !tokens::looks_like_hex_secret(&body.token) {
        return Err(AccountsError::TokenInvalid);
    }
    new_password(&body.password)?;
    let hash = accounts.passwords().hash(body.password).await?;
    match store::reset_password(accounts.db(), tokens::digest(&body.token), &hash).await? {
        Some(user) => {
            tracing::info!(user = %user, "password reset; every session ended");
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(AccountsError::TokenInvalid),
    }
}

/// The budgets and the verify, shared by sign-in and account deletion: per address first,
/// then (for an untrusted pair) global, then the password, against a dummy hash when the
/// account does not exist. On a right password every token comes back.
async fn check_credentials(
    accounts: &Accounts,
    address: ClientKey,
    user: Option<(Uuid, String)>,
    password: String,
) -> Result<Option<Uuid>> {
    let gate = accounts.sign_in_gate();
    let mut attempt: Attempt = gate
        .admit(address)
        .map_err(|w| AccountsError::RateLimited(w.secs()))?;
    let trusted = match &user {
        Some((id, _)) => {
            store::is_trusted(
                accounts.db(),
                *id,
                &address.to_string(),
                accounts.settings().sign_in.trust_window,
            )
            .await?
        }
        None => false,
    };
    if !trusted {
        gate.reserve_global(&mut attempt)
            .map_err(|w| AccountsError::RateLimited(w.secs()))?;
    }
    if password.is_empty() || password.len() > domain::MAX_PASSWORD_BYTES {
        return Ok(None);
    }
    let (id, hash) = match user {
        Some((id, hash)) => (Some(id), Some(hash)),
        None => (None, None),
    };
    if accounts.passwords().verify(password, hash).await? {
        gate.succeeded(attempt);
        Ok(id)
    } else {
        Ok(None)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignIn {
    email: String,
    password: String,
}

async fn sign_in(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<SignIn>,
) -> Result<Response> {
    let address = client(&accounts, peer, &headers);
    // An address that is not one gets the same answer as a wrong password, after the
    // address budget, so the budgets bound probing of either.
    let found = match domain::normalize_email(&body.email) {
        Some(email) => store::user_by_email(accounts.db(), &email).await?,
        None => None,
    };
    let verified = found.as_ref().is_some_and(|u| u.verified);
    let checked = check_credentials(
        &accounts,
        address,
        found.map(|u| (u.id, u.password_hash)),
        body.password,
    )
    .await?;
    let Some(user) = checked else {
        store::event(accounts.db(), "session.refused", None, None).await?;
        tracing::info!("sign-in refused");
        return Err(AccountsError::SignInFailed);
    };
    if !verified {
        return Err(AccountsError::EmailUnverified);
    }
    let session = tokens::new_hex_secret().map_err(internal)?;
    let settings = accounts.settings();
    store::signed_in(
        accounts.db(),
        user,
        &address.to_string(),
        tokens::digest(&session),
        settings.session_ttl,
    )
    .await?;
    tracing::info!(user = %user, "signed in");
    let secure = if settings.cookie_secure {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{}={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}",
        accounts.cookie_name(),
        settings.session_ttl.as_secs()
    );
    let mut response = (StatusCode::OK, Json(json!({ "user_id": user }))).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(internal)?,
    );
    no_store(&mut response);
    Ok(response)
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

async fn sign_out(State(accounts): State<Accounts>, headers: HeaderMap) -> Result<Response> {
    if let Some(session) = super::session_cookie(&headers, accounts.cookie_name())
        && let Some(user) = store::delete_session(accounts.db(), tokens::digest(session)).await?
    {
        store::event(accounts.db(), "session.ended", Some(user), None).await?;
    }
    let secure = if accounts.settings().cookie_secure {
        "; Secure"
    } else {
        ""
    };
    let clear = format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{secure}",
        accounts.cookie_name()
    );
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear).map_err(internal)?,
    );
    Ok(response)
}

async fn me(State(accounts): State<Accounts>, headers: HeaderMap) -> Result<Response> {
    let user = accounts.person(&headers).await?;
    let (email, verified, workspaces) = store::me(accounts.db(), user)
        .await?
        .ok_or(AccountsError::Unauthenticated)?;
    let mut response = Json(
        json!({ "user_id": user, "email": email, "verified": verified, "workspaces": workspaces }),
    )
    .into_response();
    no_store(&mut response);
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reauthenticate {
    password: String,
}

/// Deletes the caller's account after re-checking the password (with the sign-in budgets).
async fn delete_me(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Reauthenticate>,
) -> Result<StatusCode> {
    let user = accounts.person(&headers).await?;
    let hash = store::password_hash(accounts.db(), user)
        .await?
        .ok_or(AccountsError::Unauthenticated)?;
    let address = client(&accounts, peer, &headers);
    if check_credentials(&accounts, address, Some((user, hash)), body.password).await? != Some(user)
    {
        return Err(AccountsError::SignInFailed);
    }
    store::delete_account(accounts.db(), user).await?;
    tracing::info!(user = %user, "account deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameBody {
    name: String,
}

fn name(raw: &str) -> Result<&str> {
    let name = raw.trim();
    if (1..=100).contains(&name.chars().count()) && !name.chars().any(char::is_control) {
        Ok(name)
    } else {
        Err(AccountsError::Invalid(
            "invalid_name",
            "a name has 1 to 100 characters",
        ))
    }
}

async fn create_workspace(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Json(body): Json<NameBody>,
) -> Result<Response> {
    let user = accounts.person(&headers).await?;
    let id = store::create_workspace(
        accounts.db(),
        user,
        name(&body.name)?,
        accounts.settings().plan,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "role": Role::Owner })),
    )
        .into_response())
}

fn member(via: Via) -> Result<(Uuid, Role)> {
    match via {
        Via::Member { user, role } => Ok((user, role)),
        Via::Token { .. } => Err(AccountsError::SessionRequired),
    }
}

async fn read_workspace(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    let access = accounts.access(&headers, workspace, Action::Read).await?;
    let (name, plan) = store::workspace(accounts.db(), workspace)
        .await?
        .ok_or(AccountsError::NotFound)?;
    let role = match access.via {
        Via::Member { role, .. } => Some(role),
        Via::Token { .. } => None,
    };
    Ok(Json(
        json!({ "id": workspace, "name": name, "plan": plan, "role": role }),
    ))
}

async fn delete_workspace(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
) -> Result<StatusCode> {
    let (user, _) = member(
        accounts
            .access(&headers, workspace, Action::Delete)
            .await?
            .via,
    )?;
    store::delete_workspace(accounts.db(), workspace, user).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_members(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    member(
        accounts
            .access(&headers, workspace, Action::ListMembers)
            .await?
            .via,
    )?;
    Ok(Json(
        json!({ "members": store::members(accounts.db(), workspace).await? }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddMember {
    email: String,
    role: Role,
}

async fn add_member(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
    Json(body): Json<AddMember>,
) -> Result<Response> {
    let action = if body.role == Role::Owner {
        Action::ManageOwners
    } else {
        Action::ManageMembers
    };
    let (actor, _) = member(accounts.access(&headers, workspace, action).await?.via)?;
    let added = store::add_member(
        accounts.db(),
        workspace,
        &email(&body.email)?,
        body.role,
        actor,
        accounts.settings().plan,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user_id": added, "role": body.role })),
    )
        .into_response())
}

async fn remove_member(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path((workspace, user)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    // Anyone may leave; removing someone else takes ManageMembers (and an owner, for an owner).
    let access = accounts.access(&headers, workspace, Action::Read).await?;
    let (actor, role) = member(access.via)?;
    if actor != user && !domain::allows(role, Action::ManageMembers) {
        return Err(AccountsError::Forbidden);
    }
    store::remove_member(
        accounts.db(),
        workspace,
        user,
        actor,
        if actor == user { Role::Owner } else { role },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_tokens(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    member(
        accounts
            .access(&headers, workspace, Action::ManageTokens)
            .await?
            .via,
    )?;
    Ok(Json(
        json!({ "tokens": store::tokens(accounts.db(), workspace).await? }),
    ))
}

async fn create_token(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path(workspace): Path<Uuid>,
    Json(body): Json<NameBody>,
) -> Result<Response> {
    let (actor, _) = member(
        accounts
            .access(&headers, workspace, Action::ManageTokens)
            .await?
            .via,
    )?;
    let (token, prefix) = tokens::new_api_token().map_err(internal)?;
    let id = store::create_token(
        accounts.db(),
        &store::NewToken {
            workspace,
            actor,
            name: name(&body.name)?,
            prefix: &prefix,
            digest: tokens::digest(&token),
        },
        accounts.settings().plan,
    )
    .await?;
    tracing::info!(token = %id, workspace = %workspace, "API token created");
    let mut response = (
        StatusCode::CREATED,
        Json(json!({ "id": id, "prefix": prefix, "token": token })),
    )
        .into_response();
    no_store(&mut response);
    Ok(response)
}

async fn delete_token(
    State(accounts): State<Accounts>,
    headers: HeaderMap,
    Path((workspace, token)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let (actor, _) = member(
        accounts
            .access(&headers, workspace, Action::ManageTokens)
            .await?
            .via,
    )?;
    store::delete_token(accounts.db(), workspace, token, actor).await?;
    Ok(StatusCode::NO_CONTENT)
}
