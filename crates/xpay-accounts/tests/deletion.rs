//! Deletion: an account takes every row the kit holds for it, except the security log's
//! events, which keep only its id (resolving to nobody afterwards); a workspace takes its
//! members and tokens; a sole owner with members must transfer first.
#![expect(
    clippy::too_many_lines,
    reason = "an end-to-end scenario reads top to bottom"
)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit, PASSWORD};
use serde_json::json;
use xpay_db::testing::Role;

const PEER: &str = "203.0.113.20:4000";

#[tokio::test]
async fn deleting_an_account_removes_every_row_the_kit_holds() {
    let kit = Kit::new().await;
    let ada = kit.verified_user("ada@example.test").await;
    kit.verified_user("bob@example.test").await;
    let ada_session = kit.session(PEER, "ada@example.test").await;
    let bob_session = kit.session(PEER, "bob@example.test").await;
    let alone = kit.workspace(&ada_session, "Ada alone").await;
    kit.token(&ada_session, alone).await;
    let shared = kit.workspace(&bob_session, "Bob's").await;
    kit.call(
        PEER,
        Method::POST,
        &format!("/v1/workspaces/{shared}/members"),
        Auth::Session(&bob_session),
        Some(json!({ "email": "ada@example.test", "role": "admin" })),
    )
    .await;
    kit.token(&ada_session, shared).await;
    // Open reset and verification links too.
    kit.call(
        PEER,
        Method::POST,
        "/v1/password-reset",
        Auth::None,
        Some(json!({ "email": "ada@example.test" })),
    )
    .await;

    let wrong = kit
        .call(
            PEER,
            Method::DELETE,
            "/v1/me",
            Auth::Session(&ada_session),
            Some(json!({ "password": "not it, not it at all" })),
        )
        .await;
    assert_eq!(
        wrong.status,
        StatusCode::UNAUTHORIZED,
        "deletion re-checks the password"
    );
    let deleted = kit
        .call(
            PEER,
            Method::DELETE,
            "/v1/me",
            Auth::Session(&ada_session),
            Some(json!({ "password": PASSWORD })),
        )
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);

    // Scan every table the kit owns, as the owner, for the id and the email.
    let owner = kit.db.connect_as(Role::Owner).await;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND relkind = 'r' AND relname <> '_sqlx_migrations' ORDER BY 1",
    )
    .fetch_all(&owner)
    .await
    .unwrap();
    for table in tables {
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT t::text FROM public.{table} t"
        )))
        .fetch_all(&owner)
        .await
        .unwrap();
        assert!(
            !rows.iter().any(|r| r.contains("ada@example.test")),
            "{table} still holds the email: {rows:?}"
        );
        if table != "account_events" {
            assert!(
                !rows.iter().any(|r| r.contains(&ada.to_string())),
                "{table} still holds the account: {rows:?}"
            );
        }
    }
    let gone: bool =
        sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM workspaces WHERE id = $1)")
            .bind(alone)
            .fetch_one(&owner)
            .await
            .unwrap();
    assert!(gone, "the workspace only Ada belonged to went with her");
    let kept: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM workspaces WHERE id = $1)")
        .bind(shared)
        .fetch_one(&owner)
        .await
        .unwrap();
    assert!(kept, "Bob's workspace stays");
    owner.close().await;
    assert_eq!(
        kit.call(
            PEER,
            Method::GET,
            "/v1/me",
            Auth::Session(&ada_session),
            None
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    kit.db.cleanup().await;
}

#[tokio::test]
async fn a_sole_owner_with_members_must_transfer_first_and_a_workspace_takes_its_rows() {
    let kit = Kit::new().await;
    kit.verified_user("ada@example.test").await;
    kit.verified_user("bob@example.test").await;
    let ada = kit.session(PEER, "ada@example.test").await;
    let w = kit.workspace(&ada, "W").await;
    kit.call(
        PEER,
        Method::POST,
        &format!("/v1/workspaces/{w}/members"),
        Auth::Session(&ada),
        Some(json!({ "email": "bob@example.test", "role": "member" })),
    )
    .await;
    let blocked = kit
        .call(
            PEER,
            Method::DELETE,
            "/v1/me",
            Auth::Session(&ada),
            Some(json!({ "password": PASSWORD })),
        )
        .await;
    assert_eq!(
        (blocked.status, blocked.code()),
        (StatusCode::CONFLICT, Some("transfer_ownership_first"))
    );

    let (_, token) = kit.token(&ada, w).await;
    let removed = kit
        .call(
            PEER,
            Method::DELETE,
            &format!("/v1/workspaces/{w}"),
            Auth::Session(&ada),
            None,
        )
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(
        kit.call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{w}"),
            Auth::Token(&token),
            None
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let owner = kit.db.connect_as(Role::Owner).await;
    let left: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM members WHERE workspace_id = $1) + (SELECT count(*) FROM api_tokens WHERE workspace_id = $1)")
        .bind(w).fetch_one(&owner).await.unwrap();
    assert_eq!(left, 0);
    owner.close().await;
    kit.db.cleanup().await;
}
