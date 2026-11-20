//! The startup check refuses a role that could switch the protections off, and a database
//! whose protections were weakened in any one way — each weakening alone, on a fresh copy
//! (Payment Sandbox D-058: names were not enough, so bodies and definitions are pinned too).

use xpay_db::guard::{Refusal, check_runtime, protection_problems};
use xpay_db::testing::{Role, TestDb};

#[tokio::test]
async fn a_freshly_migrated_database_is_protected_and_the_runtime_role_may_run() {
    let db = TestDb::new().await;
    assert_eq!(
        protection_problems(&db.pool).await.unwrap(),
        Vec::<String>::new()
    );
    check_runtime(&db.pool).await.unwrap();
    db.cleanup().await;
}

#[tokio::test]
async fn the_server_refuses_to_run_as_a_superuser_or_as_the_owner() {
    let db = TestDb::new().await;
    let admin = db.connect_as(Role::Admin).await;
    assert!(matches!(
        check_runtime(&admin).await,
        Err(Refusal::Superuser(_))
    ));
    let owner = db.connect_as(Role::Owner).await;
    assert!(matches!(
        check_runtime(&owner).await,
        Err(Refusal::Owner(_))
    ));
    admin.close().await;
    owner.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn the_server_refuses_a_role_with_createrole() {
    let db = TestDb::new().await;
    // A role of this test's own: roles are cluster-wide and xpay_app is shared by every
    // test running in parallel.
    let probe = format!("xpay_probe_{}", db.name().trim_start_matches("xpay_test_"));
    db.exec_as(
        Role::Admin,
        &format!(
            "CREATE ROLE {probe} LOGIN CREATEROLE PASSWORD 'probe-password-0123456789';
             GRANT CONNECT ON DATABASE {name} TO {probe};",
            name = db.name()
        ),
    )
    .await;
    let url = db.url_as(Role::Runtime).replace(
        "xpay_app:xpay-app-dev-password",
        &format!("{probe}:probe-password-0123456789"),
    );
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let refused = check_runtime(&pool).await;
    assert!(
        matches!(
            refused,
            Err(Refusal::TooBroad {
                what: "CREATEROLE",
                ..
            })
        ),
        "{refused:?}"
    );
    pool.close().await;
    db.exec_as(
        Role::Admin,
        &format!(
            "REVOKE CONNECT ON DATABASE {} FROM {probe}; DROP ROLE {probe}",
            db.name()
        ),
    )
    .await;
    db.cleanup().await;
}

/// Each weakening: what it is, who does it, the SQL, and the problem the manifest must name.
const WEAKENINGS: [(&str, Role, &str, &str); 13] = [
    (
        "drop an append-only trigger",
        Role::Owner,
        "DROP TRIGGER account_events_append_only ON account_events",
        "account_events_append_only is missing",
    ),
    (
        "disable a trigger",
        Role::Owner,
        "ALTER TABLE account_events DISABLE TRIGGER account_events_no_truncate",
        "account_events_no_truncate is missing or not enabled",
    ),
    (
        "enable a trigger for origin only (replica mode silences it)",
        Role::Owner,
        "ALTER TABLE account_events ENABLE TRIGGER account_events_stamp",
        "account_events_stamp is missing or not enabled",
    ),
    (
        "replace a function's body with a no-op, search path still pinned",
        Role::Owner,
        "CREATE OR REPLACE FUNCTION public.refuse_change() RETURNS trigger LANGUAGE plpgsql \
          SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$ BEGIN RETURN NULL; END $$",
        "function refuse_change is not defined as the migrations left it",
    ),
    (
        "unpin a function's search path",
        Role::Owner,
        "ALTER FUNCTION public.stamp_event() RESET search_path",
        "stamp_event does not pin search_path",
    ),
    (
        "make a function SECURITY DEFINER",
        Role::Owner,
        "ALTER FUNCTION public.refuse_change() SECURITY DEFINER",
        "refuse_change is SECURITY DEFINER",
    ),
    (
        "add an unlisted function",
        Role::Owner,
        "CREATE FUNCTION public.helper() RETURNS int LANGUAGE sql \
          SET search_path = pg_catalog, pg_temp AS 'SELECT 1'",
        "function helper is not defined as the migrations left it",
    ),
    (
        "add an unlisted trigger",
        Role::Owner,
        "CREATE TRIGGER users_extra BEFORE DELETE ON users FOR EACH ROW EXECUTE FUNCTION refuse_change()",
        "trigger users.users_extra is not in the manifest",
    ),
    (
        "grant the runtime role UPDATE on the security log",
        Role::Owner,
        "GRANT UPDATE ON account_events TO xpay_app",
        "xpay_app holds [INSERT,SELECT,UPDATE] on account_events",
    ),
    (
        "grant the runtime role TRUNCATE on a mutable table",
        Role::Owner,
        "GRANT TRUNCATE ON sessions TO xpay_app",
        "xpay_app holds [DELETE,INSERT,SELECT,TRUNCATE,UPDATE] on sessions",
    ),
    (
        "grant the runtime role TEMP",
        Role::Admin,
        "GRANT TEMP ON DATABASE {db} TO xpay_app",
        "xpay_app may create temporary tables",
    ),
    (
        "grant the runtime role CREATE on the schema",
        Role::Owner,
        "GRANT CREATE ON SCHEMA public TO xpay_app",
        "xpay_app may create objects",
    ),
    (
        "hand a table to another owner",
        Role::Admin,
        "ALTER TABLE invites OWNER TO {admin}",
        "table invites is owned by",
    ),
];

#[tokio::test]
async fn every_weakening_alone_is_refused_at_startup() {
    for (what, role, sql, expected) in WEAKENINGS {
        let db = TestDb::new().await;
        let admin_name: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&db.connect_as(Role::Admin).await)
            .await
            .unwrap();
        let sql = sql
            .replace("{db}", db.name())
            .replace("{admin}", &admin_name);
        db.exec_as(role, &sql).await;
        let refused = check_runtime(&db.pool).await;
        match &refused {
            Err(Refusal::Unprotected(problems)) => assert!(
                problems.contains(expected),
                "{what}: refused, but for {problems:?}, not {expected:?}"
            ),
            other => panic!("{what}: not refused as unprotected: {other:?}"),
        }
        db.cleanup().await;
    }
}

#[tokio::test]
async fn the_security_log_refuses_rewrites_even_from_its_owner() {
    let db = TestDb::new().await;
    sqlx::query("INSERT INTO account_events (id, at, kind) VALUES (gen_random_uuid(), '2000-01-01', 'test.event')")
        .execute(&db.pool)
        .await
        .unwrap();
    // Stamped with the database's clock, never the caller's.
    let backdated: bool =
        sqlx::query_scalar("SELECT at < now() - interval '1 day' FROM account_events")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!backdated, "the runtime role backdated an event");
    let owner = db.connect_as(Role::Owner).await;
    for sql in [
        "UPDATE account_events SET kind = 'rewritten'",
        "DELETE FROM account_events",
        "TRUNCATE account_events",
    ] {
        let refused = sqlx::query(sql).execute(&owner).await;
        assert!(refused.is_err(), "{sql} was allowed for the owner");
    }
    owner.close().await;
    // Replica mode (a superuser's switch) does not silence ALWAYS triggers.
    let admin = db.connect_as(Role::Admin).await;
    let mut tx = admin.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query("DELETE FROM account_events")
            .execute(&mut *tx)
            .await
            .is_err(),
        "replica mode silenced the append-only trigger"
    );
    drop(tx);
    admin.close().await;
    db.cleanup().await;
}
