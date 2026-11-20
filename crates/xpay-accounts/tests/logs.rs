//! The log-capture test (D-015 pattern): every flow of the kit runs with TRACE logging
//! captured, then the log is scanned for every secret the flows handled — passwords, the
//! invite code, the verification and reset links' tokens, the session id, the API token —
//! and the email addresses. The scan proves itself on each needle first
//! (`xpay_db::logcheck`), and the log must still say what happened.
#![expect(
    clippy::too_many_lines,
    reason = "an end-to-end scenario reads top to bottom"
)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit, PASSWORD, session_from};
use serde_json::json;
use xpay_db::logcheck::{assert_no_leaks, capture_global};

const PEER: &str = "203.0.113.30:4000";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_flow_of_the_kit_logs_a_secret_or_an_address() {
    let captured = capture_global();
    let kit = Kit::new().await;
    let invite = kit.invite().await;
    let email = "leaky.lovelace@example.test";
    kit.call(
        PEER,
        Method::POST,
        "/v1/signup",
        Auth::None,
        Some(json!({ "invite": invite, "email": email, "password": PASSWORD })),
    )
    .await;
    let verify = kit.sink.latest_to(email).await.unwrap().token().unwrap();
    kit.call(
        PEER,
        Method::POST,
        "/v1/verify",
        Auth::None,
        Some(json!({ "token": verify })),
    )
    .await;
    let wrong = "a wrong password guessed here";
    kit.call(
        PEER,
        Method::POST,
        "/v1/session",
        Auth::None,
        Some(json!({ "email": email, "password": wrong })),
    )
    .await;
    let signed = kit
        .call(
            PEER,
            Method::POST,
            "/v1/session",
            Auth::None,
            Some(json!({ "email": email, "password": PASSWORD })),
        )
        .await;
    assert_eq!(signed.status, StatusCode::OK);
    let session = session_from(&signed).unwrap();
    let w = kit.workspace(&session, "Logged workspace").await;
    let (_, token) = kit.token(&session, w).await;
    kit.call(
        PEER,
        Method::GET,
        &format!("/v1/workspaces/{w}/hello"),
        Auth::Token(&token),
        None,
    )
    .await;
    kit.call(
        PEER,
        Method::GET,
        &format!("/v1/workspaces/{w}"),
        Auth::Token("st_forgedforgedf_forgedforgedforgedforgedforgedf"),
        None,
    )
    .await;
    kit.call(
        PEER,
        Method::POST,
        "/v1/password-reset",
        Auth::None,
        Some(json!({ "email": email })),
    )
    .await;
    let reset = kit.sink.latest_to(email).await.unwrap().token().unwrap();
    let new_password = "the new password after reset";
    kit.call(
        PEER,
        Method::POST,
        "/v1/password-reset/confirm",
        Auth::None,
        Some(json!({ "token": reset, "password": new_password })),
    )
    .await;
    let session = kit.session_with(email, new_password).await;
    let deleted = kit
        .call(
            PEER,
            Method::DELETE,
            "/v1/me",
            Auth::Session(&session),
            Some(json!({ "password": new_password })),
        )
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);

    let log = captured.text();
    assert_no_leaks(
        &log,
        &[
            PASSWORD,
            wrong,
            new_password,
            &invite,
            &verify,
            &reset,
            &session,
            &token,
            email,
            "leaky.lovelace",
        ],
        "the accounts kit's flows",
    );
    assert!(
        log.contains("signed in") && log.contains("account deleted"),
        "the log says nothing useful"
    );
    kit.db.cleanup().await;
}

trait SessionWith {
    async fn session_with(&self, email: &str, password: &str) -> String;
}

impl SessionWith for Kit {
    async fn session_with(&self, email: &str, password: &str) -> String {
        let answer = self
            .call(
                PEER,
                Method::POST,
                "/v1/session",
                Auth::None,
                Some(json!({ "email": email, "password": password })),
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        session_from(&answer).unwrap_or_default()
    }
}
