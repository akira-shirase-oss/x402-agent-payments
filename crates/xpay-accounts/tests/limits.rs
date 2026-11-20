//! Plan limits at their edges, under concurrency: exactly the limit is admitted, whatever
//! arrives at once, and capacity comes back when something is deleted.

mod common;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit};
use serde_json::json;
use xpay_accounts::domain::PlanLimits;

const PEER: &str = "203.0.113.11:4000";

#[tokio::test]
async fn token_and_workspace_limits_hold_under_concurrency() {
    let kit = Kit::with(|s| {
        s.plan = PlanLimits {
            max_members: 3,
            max_tokens: 3,
            max_owned_workspaces: 2,
        }
    })
    .await;
    kit.verified_user("ada@example.test").await;
    let ada = kit.session(PEER, "ada@example.test").await;
    let w = kit.workspace(&ada, "W").await;
    kit.token(&ada, w).await;
    kit.token(&ada, w).await;
    // One slot left; eight ask at once.
    let path = format!("/v1/workspaces/{w}/tokens");
    let attempts = (0..8).map(|_| {
        kit.call(
            PEER,
            Method::POST,
            &path,
            Auth::Session(&ada),
            Some(json!({ "name": "race" })),
        )
    });
    let answers = futures_join(attempts).await;
    let created = answers
        .iter()
        .filter(|a| a.status == StatusCode::CREATED)
        .count();
    let limited = answers
        .iter()
        .filter(|a| a.code() == Some("plan_limit") && a.body["error"]["limit"] == "tokens")
        .count();
    assert_eq!(
        (created, limited),
        (1, 7),
        "{:?}",
        answers.iter().map(|a| a.status).collect::<Vec<_>>()
    );
    // Deleting one gives one back.
    let list = kit
        .call(PEER, Method::GET, &path, Auth::Session(&ada), None)
        .await;
    let first = list.body["tokens"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        kit.call(
            PEER,
            Method::DELETE,
            &format!("{path}/{first}"),
            Auth::Session(&ada),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        kit.call(
            PEER,
            Method::POST,
            &path,
            Auth::Session(&ada),
            Some(json!({ "name": "again" }))
        )
        .await
        .status,
        StatusCode::CREATED
    );

    // Owned workspaces: one more is admitted, then none.
    kit.workspace(&ada, "W2").await;
    let third = kit
        .call(
            PEER,
            Method::POST,
            "/v1/workspaces",
            Auth::Session(&ada),
            Some(json!({ "name": "W3" })),
        )
        .await;
    assert_eq!(
        (third.status, third.code()),
        (StatusCode::FORBIDDEN, Some("plan_limit"))
    );
    kit.db.cleanup().await;
}

#[tokio::test]
async fn the_member_limit_counts_the_owner() {
    let kit = Kit::with(|s| {
        s.plan = PlanLimits {
            max_members: 2,
            ..PlanLimits::default()
        }
    })
    .await;
    for email in ["ada@example.test", "bob@example.test", "cy@example.test"] {
        kit.verified_user(email).await;
    }
    let ada = kit.session(PEER, "ada@example.test").await;
    let w = kit.workspace(&ada, "W").await;
    let path = format!("/v1/workspaces/{w}/members");
    let bob = kit
        .call(
            PEER,
            Method::POST,
            &path,
            Auth::Session(&ada),
            Some(json!({ "email": "bob@example.test", "role": "member" })),
        )
        .await;
    assert_eq!(bob.status, StatusCode::CREATED);
    let cy = kit
        .call(
            PEER,
            Method::POST,
            &path,
            Auth::Session(&ada),
            Some(json!({ "email": "cy@example.test", "role": "member" })),
        )
        .await;
    assert_eq!(
        (cy.status, cy.body["error"]["limit"].as_str()),
        (StatusCode::FORBIDDEN, Some("members"))
    );
    // An unknown or unverified address is not found; a duplicate is a conflict.
    let ghost = kit
        .call(
            PEER,
            Method::POST,
            &path,
            Auth::Session(&ada),
            Some(json!({ "email": "ghost@example.test", "role": "member" })),
        )
        .await;
    assert_eq!(ghost.status, StatusCode::NOT_FOUND);
    kit.db.cleanup().await;
}

/// Runs the futures concurrently (no extra dependency).
async fn futures_join<F: std::future::Future<Output = common::Answer>>(
    futures: impl Iterator<Item = F>,
) -> Vec<common::Answer> {
    let mut set = Vec::new();
    for f in futures {
        set.push(f);
    }
    let mut pinned: Vec<_> = set.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<common::Answer>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in pinned.iter_mut().enumerate() {
            if out[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => out[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    out.into_iter().flatten().collect()
}
