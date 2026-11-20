//! Fingerprints: equal for equal contents whatever the physical order, different for any
//! change.

use xpay_db::fingerprint::fingerprint;
use xpay_db::testing::{Role, TestDb};

#[tokio::test]
async fn fingerprints_follow_contents_not_order() {
    let a = TestDb::new().await;
    let b = TestDb::new().await;
    let rows = [
        ("00000000-0000-7000-8000-000000000001", "one@example.test"),
        ("00000000-0000-7000-8000-000000000002", "two@example.test"),
    ];
    for (id, email) in rows {
        a.exec_as(Role::Owner, &format!("INSERT INTO users (id, email, password_hash, created_at) VALUES ('{id}', '{email}', 'x', '2026-01-01T00:00:00Z')")).await;
    }
    for (id, email) in rows.iter().rev() {
        b.exec_as(Role::Owner, &format!("INSERT INTO users (id, email, password_hash, created_at) VALUES ('{id}', '{email}', 'x', '2026-01-01T00:00:00Z')")).await;
    }
    let (fa, fb) = (
        fingerprint(&a.pool).await.unwrap(),
        fingerprint(&b.pool).await.unwrap(),
    );
    assert_eq!(fa["users"].rows, 2);
    assert_eq!(fa["users"], fb["users"]);
    assert_eq!(fa["workspaces"].rows, 0);
    b.exec_as(
        Role::Owner,
        "UPDATE users SET password_hash = 'y' WHERE email = 'two@example.test'",
    )
    .await;
    assert_ne!(fingerprint(&b.pool).await.unwrap()["users"], fa["users"]);
    a.cleanup().await;
    b.cleanup().await;
}
