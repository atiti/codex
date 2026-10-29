use super::*;
use crate::SqliteConfig;
use crate::migrations::STATE_MIGRATOR;
use crate::migrations::runtime_state_migrator;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::migrate::Migration;
use std::borrow::Cow;

fn migrator_through(version: i64) -> Migrator {
    let mut migrator = runtime_state_migrator();
    migrator.migrations = Cow::Owned(
        migrator
            .iter()
            .filter(|m| m.version <= version)
            .cloned()
            .collect(),
    );
    migrator
}

fn official_migrator(frontier: i64) -> Migrator {
    let mut migrator = migrator_through(/*version*/ 59);
    let migrations = migrator.migrations.to_mut();
    for version in 60..=frontier {
        let source = STATE_MIGRATOR
            .iter()
            .find(|m| m.version == version + 2)
            .unwrap();
        migrations.push(Migration::new(
            version,
            source.description.clone(),
            source.migration_type,
            source.sql.clone(),
            source.no_tx,
        ));
    }
    migrator
}

async fn fixture() -> (impl Drop, SqliteConfig, SqlitePool) {
    let path = crate::runtime::test_support::unique_temp_dir();
    std::fs::create_dir_all(&path).unwrap();
    let temp = scopeguard::guard(path.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = SqliteConfig::new_for_testing(path.abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    (temp, sqlite, pool)
}

async fn ledger(pool: &SqlitePool) -> Vec<(i64, String, String, bool, Vec<u8>, i64)> {
    sqlx::query_as("SELECT version, description, CAST(installed_on AS TEXT), success, checksum, execution_time FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool).await.unwrap()
}

async fn seed_feedback(pool: &SqlitePool) {
    sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('t', '/tmp/t', 1, 1, 'cli', 'openai', '/tmp', 'title', 'read-only', 'never')").execute(pool).await.unwrap();
    sqlx::query("INSERT INTO guardian_review_feedback (id, thread_id, record) VALUES ('feedback', 't', X'0001FF')").execute(pool).await.unwrap();
}

async fn verify_feedback(pool: &SqlitePool) {
    let actual: Vec<u8> =
        sqlx::query_scalar("SELECT record FROM guardian_review_feedback WHERE id = 'feedback'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(actual, vec![0, 1, 255]);
}

#[tokio::test]
async fn shipped_fork_upgrade_preserves_ledger_data_and_rollback() {
    let (_temp, sqlite, pool) = fixture().await;
    let old = migrator_through(/*version*/ 63);
    old.run(&pool).await.unwrap();
    seed_feedback(&pool).await;
    sqlx::query("INSERT INTO thread_read_receipts (thread_id, first_unread_turn, revision) VALUES ('t', 'turn', 'rev')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_work (thread_id, title, summary, status, updated_at, pending_name) VALUES ('t', 'task', 'summary', 'working', 123, 'pending')").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_work_claims (resource, thread_id, expires_at) VALUES ('file:/tmp/test', 't', 456)").execute(&pool).await.unwrap();
    let before = ledger(&pool).await;
    let upgraded = sqlite
        .open_state_db(&runtime_state_migrator(), /*telemetry_override*/ None)
        .await
        .unwrap();
    assert_eq!(ledger(&upgraded).await[..before.len()], before);
    verify_feedback(&upgraded).await;
    let task: (String, String, i64) = sqlx::query_as(
        "SELECT title, pending_name, updated_at FROM task_work WHERE thread_id = 't'",
    )
    .fetch_one(&upgraded)
    .await
    .unwrap();
    assert_eq!(task, ("task".to_owned(), "pending".to_owned(), 123));
    let claim: (String, i64) =
        sqlx::query_as("SELECT resource, expires_at FROM task_work_claims WHERE thread_id = 't'")
            .fetch_one(&upgraded)
            .await
            .unwrap();
    assert_eq!(claim, ("file:/tmp/test".to_owned(), 456));
    let receipt: (String, String) =
        sqlx::query_as("SELECT first_unread_turn, revision FROM thread_read_receipts")
            .fetch_one(&upgraded)
            .await
            .unwrap();
    assert_eq!(receipt, ("turn".to_owned(), "rev".to_owned()));
    sqlx::query("UPDATE threads SET attached_thread_parent_id = 'parent' WHERE id = 't'")
        .execute(&upgraded)
        .await
        .unwrap();
    old.run(&upgraded)
        .await
        .expect("shipped fork can reopen canonical ledger");
}

#[tokio::test]
async fn official_upgrades_preserve_all_existing_records_and_data() {
    for frontier in 60..=62 {
        let (_temp, sqlite, pool) = fixture().await;
        official_migrator(frontier).run(&pool).await.unwrap();
        seed_feedback(&pool).await;
        if frontier >= 61 {
            sqlx::query(
                "INSERT INTO thread_read_receipts (thread_id, revision) VALUES ('t', 'rev')",
            )
            .execute(&pool)
            .await
            .unwrap();
        }
        if frontier == 62 {
            sqlx::query("UPDATE threads SET attached_thread_parent_id = 'parent' WHERE id = 't'")
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut before = ledger(&pool).await;
        for row in &mut before {
            if row.0 >= 60 {
                row.0 += 2;
            }
        }
        let upgraded = sqlite
            .open_state_db(&runtime_state_migrator(), /*telemetry_override*/ None)
            .await
            .unwrap();
        let after = ledger(&upgraded).await;
        for row in before {
            assert_eq!(after.iter().find(|entry| entry.0 == row.0), Some(&row));
        }
        assert_eq!(
            after.iter().map(|row| row.0).collect::<Vec<_>>(),
            (1..=64).collect::<Vec<_>>()
        );
        verify_feedback(&upgraded).await;
        if frontier >= 61 {
            let revision: String = sqlx::query_scalar("SELECT revision FROM thread_read_receipts")
                .fetch_one(&upgraded)
                .await
                .unwrap();
            assert_eq!(revision, "rev");
        }
        if frontier == 62 {
            let parent: String =
                sqlx::query_scalar("SELECT attached_thread_parent_id FROM threads WHERE id = 't'")
                    .fetch_one(&upgraded)
                    .await
                    .unwrap();
            assert_eq!(parent, "parent");
        }
        let snapshot = ledger(&upgraded).await;
        repair_upstream_165_migration_versions(&upgraded, &runtime_state_migrator())
            .await
            .unwrap();
        assert_eq!(ledger(&upgraded).await, snapshot);
    }
}

#[tokio::test]
async fn unknown_dirty_or_conflicting_ledgers_are_unchanged() {
    for mutation in [
        "UPDATE _sqlx_migrations SET checksum = X'1234' WHERE version = 60",
        "UPDATE _sqlx_migrations SET success = 0 WHERE version = 60",
        "UPDATE _sqlx_migrations SET success = 0 WHERE version = 61",
        "UPDATE _sqlx_migrations SET checksum = X'1234' WHERE version = 62",
        "INSERT INTO _sqlx_migrations SELECT 63, description, installed_on, success, checksum, execution_time FROM _sqlx_migrations WHERE version = 61",
        "INSERT INTO _sqlx_migrations SELECT 64, description, installed_on, success, checksum, execution_time FROM _sqlx_migrations WHERE version = 62",
    ] {
        let (_temp, _sqlite, pool) = fixture().await;
        official_migrator(/*frontier*/ 62).run(&pool).await.unwrap();
        seed_feedback(&pool).await;
        sqlx::query(mutation).execute(&pool).await.unwrap();
        let before = ledger(&pool).await;
        let schema: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            repair_upstream_165_migration_versions(&pool, &runtime_state_migrator())
                .await
                .is_err(),
            "{mutation}"
        );
        assert_eq!(ledger(&pool).await, before);
        let after: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT name, sql FROM sqlite_master ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(after, schema);
        verify_feedback(&pool).await;
    }
}

#[tokio::test]
async fn canonical_fork_does_not_wait_for_another_writer() {
    let (_temp, sqlite, pool) = fixture().await;
    STATE_MIGRATOR.run(&pool).await.unwrap();
    let writer = pool.begin_immediate().await.unwrap();
    let reader = sqlite
        .open_read_only_pool(&sqlite.state_db_path(), /*busy_timeout*/ None)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        repair_upstream_165_migration_versions(&reader, &runtime_state_migrator()),
    )
    .await
    .unwrap()
    .unwrap();
    writer.rollback().await.unwrap();
}

#[tokio::test]
async fn simultaneous_repairs_are_idempotent() {
    let (_temp, _sqlite, pool) = fixture().await;
    official_migrator(/*frontier*/ 62).run(&pool).await.unwrap();
    let migrator = runtime_state_migrator();
    let (first, second) = tokio::join!(
        repair_upstream_165_migration_versions(&pool, &migrator),
        repair_upstream_165_migration_versions(&pool, &migrator)
    );
    first.unwrap();
    second.unwrap();
    migrator.run(&pool).await.unwrap();
}

#[tokio::test]
async fn simultaneous_full_opens_preserve_the_complete_upgrade() {
    for _ in 0..4 {
        let (_temp, sqlite, pool) = fixture().await;
        official_migrator(/*frontier*/ 62).run(&pool).await.unwrap();
        seed_feedback(&pool).await;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
        let mut opens = Vec::new();
        for _ in 0..4 {
            let sqlite = sqlite.clone();
            let barrier = barrier.clone();
            opens.push(tokio::spawn(async move {
                barrier.wait().await;
                sqlite
                    .open_state_db(&runtime_state_migrator(), /*telemetry_override*/ None)
                    .await
            }));
        }
        for open in opens {
            let upgraded = open
                .await
                .unwrap()
                .expect("concurrent complete startup should succeed");
            verify_feedback(&upgraded).await;
        }
        assert_eq!(ledger(&pool).await.len(), 64);
    }
}

#[tokio::test]
async fn migration_failure_rolls_back_the_compatibility_rekey() {
    let (_temp, sqlite, pool) = fixture().await;
    official_migrator(/*frontier*/ 62).run(&pool).await.unwrap();
    seed_feedback(&pool).await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'1234' WHERE version = 1")
        .execute(&pool)
        .await
        .unwrap();
    let before = ledger(&pool).await;
    assert!(
        sqlite
            .open_state_db(&runtime_state_migrator(), /*telemetry_override*/ None)
            .await
            .is_err()
    );
    assert_eq!(ledger(&pool).await, before);
    verify_feedback(&pool).await;
}
