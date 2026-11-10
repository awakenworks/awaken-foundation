use super::*;
use crate::Migration;

async fn configured_pool() -> Option<PgPool> {
    let url = std::env::var("AWAKEN_TEST_POSTGRES_URL").ok()?;
    Some(PgPool::connect(&url).await.unwrap())
}

fn prefix(label: &str) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{label}_{stamp}")
}

async fn cleanup(pool: &PgPool, prefix: &str) {
    // Each test owns this validated namespace and its complete set of tables.
    sql_identifier(prefix).unwrap();
    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP TABLE {prefix}_item; DROP TABLE {prefix}_schema_migrations; \
         DROP TABLE {prefix}_schema_migrations_meta"
    )))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn independent_pools_apply_one_multistatement_bundle_once() {
    let Some(first_pool) = configured_pool().await else {
        return;
    };
    let second_pool = configured_pool().await.unwrap();
    let prefix = prefix("race");
    let first = PostgresMigrationRunner::with_prefix(first_pool.clone(), &prefix).unwrap();
    let second = PostgresMigrationRunner::with_prefix(second_pool, &prefix).unwrap();
    let bundle = MigrationBundle::new(
        "foundation.concurrent_driver",
        vec![
            Migration::new(
                1,
                "create item and index",
                "CREATE TABLE {prefix}_item (id TEXT PRIMARY KEY); \
             CREATE INDEX {prefix}_item_id ON {prefix}_item (id)",
            )
            .unwrap(),
        ],
    )
    .unwrap();
    let (left, right) = tokio::join!(first.run_bundle(&bundle), second.run_bundle(&bundle));
    let mut applied = [left.unwrap().len(), right.unwrap().len()];
    applied.sort();
    assert_eq!(applied, [0, 1]);
    first.verify_bundle(&bundle).await.unwrap();
    let receipts: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {}",
        first.ledger_table()
    )))
    .fetch_one(&first_pool)
    .await
    .unwrap();
    assert_eq!(receipts, 1);
    cleanup(&first_pool, &prefix).await;
}

#[tokio::test]
async fn failed_body_rolls_back_ddl_and_receipt_without_stranding_the_guard() {
    let Some(pool) = configured_pool().await else {
        return;
    };
    let prefix = prefix("rollback");
    let runner = PostgresMigrationRunner::with_prefix(pool.clone(), &prefix).unwrap();
    let failed = MigrationBundle::new(
        "foundation.failed_driver",
        vec![
            Migration::new(
                1,
                "fail after first statement",
                "CREATE TABLE {prefix}_item (id TEXT PRIMARY KEY); \
             INSERT INTO {prefix}_missing (id) VALUES ('never')",
            )
            .unwrap(),
        ],
    )
    .unwrap();
    assert!(matches!(
        runner.run_bundle(&failed).await,
        Err(MigrationError::Backend { .. })
    ));
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{prefix}_item"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(table.is_none());
    let receipts: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {}",
        runner.ledger_table()
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(receipts, 0);
    let recovery = MigrationBundle::new(
        "foundation.recovered_driver",
        vec![
            Migration::new(
                1,
                "create after known rollback",
                "CREATE TABLE {prefix}_item (id TEXT PRIMARY KEY)",
            )
            .unwrap(),
        ],
    )
    .unwrap();
    assert_eq!(runner.run_bundle(&recovery).await.unwrap().len(), 1);
    runner.verify_bundle(&recovery).await.unwrap();
    cleanup(&pool, &prefix).await;
}

#[tokio::test]
async fn receipt_failure_rolls_back_an_otherwise_valid_body() {
    let Some(pool) = configured_pool().await else {
        return;
    };
    let prefix = prefix("receipt");
    let runner = PostgresMigrationRunner::with_prefix(pool.clone(), &prefix).unwrap();
    runner.ensure_ledger().await.unwrap();
    // Reject only the receipt, after the otherwise valid body has executed.
    // The interpolated table is the validated runner's own ledger identifier.
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {} ADD CONSTRAINT reject_receipt CHECK (false)",
        runner.ledger_table()
    )))
    .execute(&pool)
    .await
    .unwrap();
    let bundle = MigrationBundle::new(
        "foundation.receipt_atomicity",
        vec![
            Migration::new(
                1,
                "create item before receipt failure",
                "CREATE TABLE {prefix}_item (id TEXT PRIMARY KEY); \
             INSERT INTO {prefix}_item (id) VALUES ('owned-fixture')",
            )
            .unwrap(),
        ],
    )
    .unwrap();
    assert!(matches!(
        runner.run_bundle(&bundle).await,
        Err(MigrationError::Backend {
            operation: "postgres_migration_record",
            ..
        })
    ));
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{prefix}_item"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        table.is_none(),
        "a refused receipt must also roll back the body DDL"
    );
    let receipts: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {}",
        runner.ledger_table()
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(receipts, 0);
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {} DROP CONSTRAINT reject_receipt",
        runner.ledger_table()
    )))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(runner.run_bundle(&bundle).await.unwrap().len(), 1);
    runner.verify_bundle(&bundle).await.unwrap();
    cleanup(&pool, &prefix).await;
}
