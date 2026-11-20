//! Sign-in budgets over HTTP: one address's flood bounds only that address; a distributed
//! flood drains the global budget but cannot lock out a trusted owner; a right password
//! costs nothing; mail is bounded too. Each refusal is 429 with `Retry-After`, decided
//! before any password work.

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit, PASSWORD};
use serde_json::json;
use xpay_accounts::signin::{MailRates, SignInRates};
use xpay_http::budget::Rate;

/// The default bursts, with refills slow enough that no token comes back during a test
/// however slow the machine (the counts below would otherwise depend on wall time).
async fn kit() -> Kit {
    Kit::with(|s| {
        s.sign_in = SignInRates {
            per_address: Rate::new(5, Duration::from_secs(3600)),
            global: Rate::new(20, Duration::from_secs(3600)),
            ..SignInRates::default()
        };
        s.mail = MailRates {
            per_address: Rate::new(3, Duration::from_secs(3600)),
            global: Rate::new(30, Duration::from_secs(3600)),
        };
    })
    .await
}

fn wrong(email: &str) -> serde_json::Value {
    json!({ "email": email, "password": "not the password at all" })
}

#[tokio::test]
async fn one_address_flood_bounds_only_that_address() {
    let kit = kit().await;
    kit.verified_user("owner@example.test").await;
    let mut evaluated = 0;
    let mut refused = 0;
    for _ in 0..30 {
        let answer = kit
            .call(
                "198.51.100.66:4000",
                Method::POST,
                "/v1/session",
                Auth::None,
                Some(wrong("owner@example.test")),
            )
            .await;
        match answer.status {
            StatusCode::UNAUTHORIZED => evaluated += 1,
            StatusCode::TOO_MANY_REQUESTS => {
                refused += 1;
                assert!(
                    answer.headers.get("retry-after").is_some(),
                    "429 says when to retry"
                );
            }
            other => panic!("unexpected {other}: {}", answer.body),
        }
    }
    assert_eq!(
        (evaluated, refused),
        (5, 25),
        "five guesses evaluated for one address"
    );
    // The owner, from their own address, is untouched.
    kit.session("203.0.113.10:5000", "owner@example.test").await;
    kit.db.cleanup().await;
}

#[tokio::test]
async fn a_distributed_flood_cannot_lock_out_a_trusted_owner() {
    let kit = kit().await;
    kit.verified_user("owner@example.test").await;
    // The owner's usual place becomes trusted by signing in once.
    kit.session("203.0.113.10:5000", "owner@example.test").await;

    // 40 addresses, one wrong guess each: the global budget (20) admits 20 verifications.
    let mut evaluated = 0;
    for n in 0..40 {
        let peer = format!("198.51.{}.{}:4000", 100 + n / 200, n % 200 + 1);
        let answer = kit
            .call(
                &peer,
                Method::POST,
                "/v1/session",
                Auth::None,
                Some(wrong("owner@example.test")),
            )
            .await;
        match answer.status {
            StatusCode::UNAUTHORIZED => evaluated += 1,
            StatusCode::TOO_MANY_REQUESTS => {}
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(
        evaluated, 20,
        "the global budget bounds guesses spread over addresses"
    );

    // The trusted pair skips the drained global budget.
    kit.session("203.0.113.10:5000", "owner@example.test").await;
    // A new device with the right password waits for the flood to pass: honest, not a hole.
    let new_device = kit
        .call(
            "192.0.2.200:4000",
            Method::POST,
            "/v1/session",
            Auth::None,
            Some(json!({ "email": "owner@example.test", "password": PASSWORD })),
        )
        .await;
    assert_eq!(
        (new_device.status, new_device.code()),
        (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited"))
    );
    kit.db.cleanup().await;
}

#[tokio::test]
async fn a_right_password_never_runs_out() {
    let kit = kit().await;
    kit.verified_user("owner@example.test").await;
    for _ in 0..12 {
        kit.session("203.0.113.10:5000", "owner@example.test").await;
    }
    kit.db.cleanup().await;
}

#[tokio::test]
async fn mail_a_client_can_cause_is_bounded() {
    let kit = kit().await;
    let answers: Vec<StatusCode> = {
        let mut out = Vec::new();
        for _ in 0..6 {
            out.push(
                kit.call(
                    "198.51.100.77:4000",
                    Method::POST,
                    "/v1/password-reset",
                    Auth::None,
                    Some(json!({ "email": "x@example.test" })),
                )
                .await
                .status,
            );
        }
        out
    };
    assert_eq!(
        answers
            .iter()
            .filter(|s| **s == StatusCode::ACCEPTED)
            .count(),
        3,
        "{answers:?}"
    );
    assert_eq!(
        answers
            .iter()
            .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
            .count(),
        3,
        "{answers:?}"
    );
    kit.db.cleanup().await;
}
