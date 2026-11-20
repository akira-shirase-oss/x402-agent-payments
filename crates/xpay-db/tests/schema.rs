//! Binary/schema compatibility: a binary serves only on exactly its own migrations.

use xpay_db::MIGRATOR;
use xpay_db::schema::{SchemaStatus, status};
use xpay_db::testing::{Role, TestDb};

#[tokio::test]
async fn a_migrated_database_is_current_and_an_unmigrated_one_is_behind() {
    let db = TestDb::new().await;
    let latest = MIGRATOR.iter().map(|m| m.version).max().unwrap();
    assert_eq!(
        status(&db.pool, &MIGRATOR).await.unwrap(),
        SchemaStatus::Current { version: latest }
    );
    db.cleanup().await;

    let bare = TestDb::unmigrated().await;
    let pending: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    assert_eq!(
        status(&bare.pool, &MIGRATOR).await.unwrap(),
        SchemaStatus::Behind { pending }
    );
    bare.cleanup().await;
}

#[tokio::test]
async fn a_schema_newer_than_the_binary_is_refused() {
    let db = TestDb::new().await;
    db.exec_as(
        Role::Owner,
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time)
         VALUES (99999, 'from a newer release', now(), true, '\\x00', 0)",
    )
    .await;
    let found = status(&db.pool, &MIGRATOR).await.unwrap();
    assert_eq!(
        found,
        SchemaStatus::Newer {
            unknown: vec![99999]
        }
    );
    assert!(!found.is_current());
    db.cleanup().await;
}

#[tokio::test]
async fn a_modified_or_failed_migration_is_refused() {
    let db = TestDb::new().await;
    db.exec_as(
        Role::Owner,
        "UPDATE _sqlx_migrations SET checksum = '\\x01' WHERE version = 1",
    )
    .await;
    assert_eq!(
        status(&db.pool, &MIGRATOR).await.unwrap(),
        SchemaStatus::Modified { version: 1 }
    );
    db.cleanup().await;

    let db = TestDb::new().await;
    db.exec_as(
        Role::Owner,
        "UPDATE _sqlx_migrations SET success = false WHERE version = 2",
    )
    .await;
    assert_eq!(
        status(&db.pool, &MIGRATOR).await.unwrap(),
        SchemaStatus::Failed { version: 2 }
    );
    db.cleanup().await;
}
