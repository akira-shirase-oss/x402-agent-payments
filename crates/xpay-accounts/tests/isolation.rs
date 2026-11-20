//! Cross-workspace isolation: workspace A's token, A's owner's session and A's member are
//! refused on every route of workspace B, and B is unchanged afterwards. Refusals are 404,
//! so workspace ids cannot be probed.
#![expect(
    clippy::too_many_lines,
    reason = "an end-to-end scenario reads top to bottom"
)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Auth, Kit};
use serde_json::json;

const PEER: &str = "203.0.113.9:4000";

#[tokio::test]
async fn workspace_a_credentials_are_refused_on_every_route_of_workspace_b() {
    let kit = Kit::new().await;
    kit.verified_user("alice@example.test").await;
    let bob = kit.verified_user("bob@example.test").await;
    kit.verified_user("carol@example.test").await;
    let alice = kit.session(PEER, "alice@example.test").await;
    let bob_session = kit.session(PEER, "bob@example.test").await;
    let carol = kit.session(PEER, "carol@example.test").await;
    let a = kit.workspace(&alice, "A").await;
    let b = kit.workspace(&bob_session, "B").await;
    let added = kit
        .call(
            PEER,
            Method::POST,
            &format!("/v1/workspaces/{a}/members"),
            Auth::Session(&alice),
            Some(json!({ "email": "carol@example.test", "role": "admin" })),
        )
        .await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.body);
    let (_, a_token) = kit.token(&alice, a).await;
    let (b_token_id, b_token) = kit.token(&bob_session, b).await;

    let routes = [
        (Method::GET, format!("/v1/workspaces/{b}"), None),
        (Method::DELETE, format!("/v1/workspaces/{b}"), None),
        (Method::GET, format!("/v1/workspaces/{b}/members"), None),
        (
            Method::POST,
            format!("/v1/workspaces/{b}/members"),
            Some(json!({ "email": "carol@example.test", "role": "owner" })),
        ),
        (
            Method::DELETE,
            format!("/v1/workspaces/{b}/members/{bob}"),
            None,
        ),
        (Method::GET, format!("/v1/workspaces/{b}/tokens"), None),
        (
            Method::POST,
            format!("/v1/workspaces/{b}/tokens"),
            Some(json!({ "name": "stolen" })),
        ),
        (
            Method::DELETE,
            format!("/v1/workspaces/{b}/tokens/{b_token_id}"),
            None,
        ),
        (Method::GET, format!("/v1/workspaces/{b}/hello"), None),
    ];
    let principals = [
        ("A's token", Auth::Token(&a_token)),
        ("A's owner", Auth::Session(&alice)),
        ("A's member", Auth::Session(&carol)),
    ];
    for (who, auth) in principals {
        for (method, path, body) in &routes {
            let answer = kit
                .call(PEER, method.clone(), path, auth, body.clone())
                .await;
            assert_eq!(
                (answer.status, answer.code()),
                (StatusCode::NOT_FOUND, Some("not_found")),
                "{who} on {method} {path}: {}",
                answer.body
            );
        }
    }

    // B is as it was: bob is still its owner, its token still works, nothing was added.
    let members = kit
        .call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{b}/members"),
            Auth::Session(&bob_session),
            None,
        )
        .await;
    assert_eq!(
        members.body["members"].as_array().map(Vec::len),
        Some(1),
        "{}",
        members.body
    );
    let tokens = kit
        .call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{b}/tokens"),
            Auth::Session(&bob_session),
            None,
        )
        .await;
    assert_eq!(
        tokens.body["tokens"].as_array().map(Vec::len),
        Some(1),
        "{}",
        tokens.body
    );
    assert_eq!(
        kit.call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{b}/hello"),
            Auth::Token(&b_token),
            None
        )
        .await
        .status,
        StatusCode::OK
    );

    // A's token reads its own workspace, and nothing that needs a person.
    assert_eq!(
        kit.call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{a}"),
            Auth::Token(&a_token),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    let manage = kit
        .call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{a}/tokens"),
            Auth::Token(&a_token),
            None,
        )
        .await;
    assert_eq!(
        (manage.status, manage.code()),
        (StatusCode::FORBIDDEN, Some("session_required"))
    );
    // A member may read but not manage tokens; an unknown token is unauthenticated.
    kit.call(
        PEER,
        Method::POST,
        &format!("/v1/workspaces/{a}/members"),
        Auth::Session(&alice),
        Some(json!({ "email": "bob@example.test", "role": "member" })),
    )
    .await;
    let as_member = kit
        .call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{a}/tokens"),
            Auth::Session(&bob_session),
            None,
        )
        .await;
    assert_eq!(
        (as_member.status, as_member.code()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );
    let forged = format!("st_{}_{}", "a".repeat(12), "b".repeat(32));
    assert_eq!(
        kit.call(
            PEER,
            Method::GET,
            &format!("/v1/workspaces/{a}"),
            Auth::Token(&forged),
            None
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    kit.db.cleanup().await;
}
