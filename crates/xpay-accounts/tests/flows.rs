//! The invite, verification and reset flows end to end: the kit's routes, the real SMTP
//! adapter, and a fake SMTP sink that receives the links.
#![expect(
    clippy::too_many_lines,
    reason = "an end-to-end scenario reads top to bottom"
)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit, PASSWORD};
use serde_json::json;

const PEER: &str = "203.0.113.7:4000";

#[tokio::test]
async fn invite_and_verification_end_to_end_against_a_fake_smtp_sink() {
    let kit = Kit::new().await;
    let invite = kit.invite().await;
    let body = json!({ "invite": invite, "email": " Ada@Example.TEST ", "password": PASSWORD });
    let up = kit
        .call(
            PEER,
            Method::POST,
            "/v1/signup",
            Auth::None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    assert_eq!(up.body["verification_email"], "sent");
    let mail = kit
        .sink
        .latest_to("ada@example.test")
        .await
        .expect("the verification email");
    assert!(
        mail.data.contains("https://app.example.test/verify?token="),
        "{}",
        mail.data
    );
    let token = mail.token().unwrap();

    // The right password before verification: refused, and says why.
    let early = kit
        .call(
            PEER,
            Method::POST,
            "/v1/session",
            Auth::None,
            Some(json!({ "email": "ada@example.test", "password": PASSWORD })),
        )
        .await;
    assert_eq!(
        (early.status, early.code()),
        (StatusCode::FORBIDDEN, Some("email_unverified"))
    );

    let verified = kit
        .call(
            PEER,
            Method::POST,
            "/v1/verify",
            Auth::None,
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(verified.status, StatusCode::NO_CONTENT);
    let again = kit
        .call(
            PEER,
            Method::POST,
            "/v1/verify",
            Auth::None,
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(
        (again.status, again.code()),
        (StatusCode::BAD_REQUEST, Some("token_invalid")),
        "a link works once"
    );

    let session = kit.session(PEER, "ADA@example.test").await;
    let me = kit
        .call(PEER, Method::GET, "/v1/me", Auth::Session(&session), None)
        .await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(
        (me.body["email"].as_str(), me.body["verified"].as_bool()),
        (Some("ada@example.test"), Some(true))
    );

    // A used invite is refused; a taken email leaves the new invite unclaimed.
    let reused = kit
        .call(PEER, Method::POST, "/v1/signup", Auth::None, Some(body))
        .await;
    assert_eq!(
        (reused.status, reused.code()),
        (StatusCode::FORBIDDEN, Some("invite_invalid"))
    );
    // Sign-ups send mail, which is budgeted per address (3, then one every 2 minutes):
    // the rest of the checks come from another address.
    let elsewhere = "203.0.113.8:4000";
    let fresh = kit.invite().await;
    let duplicate = kit
        .call(
            elsewhere,
            Method::POST,
            "/v1/signup",
            Auth::None,
            Some(json!({ "invite": fresh, "email": "ada@example.test", "password": PASSWORD })),
        )
        .await;
    assert_eq!(
        (duplicate.status, duplicate.code()),
        (StatusCode::CONFLICT, Some("email_taken"))
    );
    let other = kit
        .call(
            elsewhere,
            Method::POST,
            "/v1/signup",
            Auth::None,
            Some(json!({ "invite": fresh, "email": "grace@example.test", "password": PASSWORD })),
        )
        .await;
    assert_eq!(
        other.status,
        StatusCode::CREATED,
        "the invite was consumed by a failed sign-up: {}",
        other.body
    );

    // A short password and a bad address are refused before any work.
    let weak = kit
        .call(elsewhere, Method::POST, "/v1/signup", Auth::None,
              Some(json!({ "invite": kit.invite().await, "email": "x@example.test", "password": "short" })))
        .await;
    assert_eq!(weak.code(), Some("weak_password"));
    kit.db.cleanup().await;
}

#[tokio::test]
async fn a_password_reset_ends_every_session() {
    let kit = Kit::new().await;
    kit.verified_user("ada@example.test").await;
    let session = kit.session(PEER, "ada@example.test").await;

    let asked = kit
        .call(
            PEER,
            Method::POST,
            "/v1/password-reset",
            Auth::None,
            Some(json!({ "email": "ada@example.test" })),
        )
        .await;
    assert_eq!(asked.status, StatusCode::ACCEPTED);
    let mail = kit.sink.latest_to("ada@example.test").await.unwrap();
    assert!(mail.data.contains("/reset?token="), "{}", mail.data);
    let new_password = "a different long passphrase";
    let confirmed = kit
        .call(
            PEER,
            Method::POST,
            "/v1/password-reset/confirm",
            Auth::None,
            Some(json!({ "token": mail.token().unwrap(), "password": new_password })),
        )
        .await;
    assert_eq!(
        confirmed.status,
        StatusCode::NO_CONTENT,
        "{}",
        confirmed.body
    );

    let old = kit
        .call(PEER, Method::GET, "/v1/me", Auth::Session(&session), None)
        .await;
    assert_eq!(
        old.status,
        StatusCode::UNAUTHORIZED,
        "the session survived the reset"
    );
    let with_old = kit
        .call(
            PEER,
            Method::POST,
            "/v1/session",
            Auth::None,
            Some(json!({ "email": "ada@example.test", "password": PASSWORD })),
        )
        .await;
    assert_eq!(with_old.status, StatusCode::UNAUTHORIZED);
    let with_new = kit
        .call(
            PEER,
            Method::POST,
            "/v1/session",
            Auth::None,
            Some(json!({ "email": "ada@example.test", "password": new_password })),
        )
        .await;
    assert_eq!(with_new.status, StatusCode::OK);

    // An unknown address gets the same answer and no mail.
    let before = kit.sink.messages().len();
    let unknown = kit
        .call(
            PEER,
            Method::POST,
            "/v1/password-reset",
            Auth::None,
            Some(json!({ "email": "nobody@example.test" })),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::ACCEPTED);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(kit.sink.messages().len(), before);
    kit.db.cleanup().await;
}

#[tokio::test]
async fn sessions_end_when_idle_and_at_sign_out() {
    let kit = Kit::with(|s| s.session_idle = std::time::Duration::from_secs(2)).await;
    kit.verified_user("ada@example.test").await;
    let session = kit.session(PEER, "ada@example.test").await;
    assert_eq!(
        kit.call(PEER, Method::GET, "/v1/me", Auth::Session(&session), None)
            .await
            .status,
        StatusCode::OK
    );
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(
        kit.call(PEER, Method::GET, "/v1/me", Auth::Session(&session), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "an idle session was still accepted"
    );

    let session = kit.session(PEER, "ada@example.test").await;
    let out = kit
        .call(
            PEER,
            Method::DELETE,
            "/v1/session",
            Auth::Session(&session),
            None,
        )
        .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(
        out.headers
            .get("set-cookie")
            .is_some_and(|c| c.to_str().unwrap_or("").contains("Max-Age=0"))
    );
    assert_eq!(
        kit.call(PEER, Method::GET, "/v1/me", Auth::Session(&session), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    kit.db.cleanup().await;
}
