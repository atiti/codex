//! Reconcile the official 0.165 state ledger with the already-shipped fork ledger.
//! The SQL is identical, but upstream assigned versions 60–62 to our 62–64.

use crate::sqlite::SqlitePoolWriteExt;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

type AppliedMigration = (i64, Vec<u8>, bool);

async fn applied(connection: &mut SqliteConnection) -> anyhow::Result<Vec<AppliedMigration>> {
    Ok(sqlx::query_as(
        "SELECT version, checksum, success FROM _sqlx_migrations WHERE version BETWEEN 60 AND 64 ORDER BY version",
    )
    .fetch_all(connection)
    .await?)
}

fn needs_repair(rows: &[AppliedMigration], migrator: &Migrator) -> anyhow::Result<bool> {
    let checksum = |version| {
        migrator
            .iter()
            .find(|migration| migration.version == version)
            .map(|migration| migration.checksum.as_ref())
    };
    let Some((_, first_checksum, success)) = rows.first() else {
        return Ok(false);
    };
    // Normal fork databases must not acquire a writer lock on every startup.
    // SQLx still validates their checksums and dirty records in the usual way.
    if rows[0].0 != 60 || Some(first_checksum.as_slice()) == checksum(60) {
        return Ok(false);
    }
    anyhow::ensure!(
        *success && Some(first_checksum.as_slice()) == checksum(62),
        "unrecognized state migration 60; refusing upstream 0.165 ledger repair"
    );
    for (index, (version, recorded_checksum, success)) in rows.iter().enumerate() {
        anyhow::ensure!(
            *version == 60 + index as i64
                && *version <= 62
                && *success
                && Some(recorded_checksum.as_slice()) == checksum(version + 2),
            "conflicting or dirty upstream 0.165 state ledger; refusing repair"
        );
    }
    Ok(true)
}

pub(crate) async fn repair_upstream_165_migration_versions(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let mut connection = pool.acquire().await?;
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !exists || !needs_repair(&applied(&mut connection).await?, migrator)? {
        return Ok(());
    }
    drop(connection);

    let mut transaction = pool.begin_immediate().await?;
    // Another process may have repaired the ledger while we waited for admission.
    if needs_repair(&applied(&mut transaction).await?, migrator)? {
        // Move 62 out of the way before 60 takes its place. No application SQL is
        // replayed and the original checksums, timestamps, and descriptions survive.
        for version in [62_i64, 60, 61] {
            sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
                .bind(version + 2)
                .bind(version)
                .execute(&mut *transaction)
                .await?;
        }
    }
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
