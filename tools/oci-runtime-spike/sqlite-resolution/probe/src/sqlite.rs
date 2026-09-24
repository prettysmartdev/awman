use std::time::Duration;

use anyhow::{ensure, Result};
use awman::data::fs::{api_db::SqliteSessionStore, task_store::TaskStore};
use microsandbox_db::pool::DbPools;
use microsandbox_migration::{Migrator, MigratorTrait};
use rusqlite::Connection;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

pub async fn check() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(&directory.path().join("api"))?;
    store.insert_session("sqlite-spike", "/fake/workdir", "2026-09-24T00:00:00Z")?;
    ensure!(store.get_session("sqlite-spike")?.is_some());
    let tasks = TaskStore::open(&directory.path().join("tasks.db"))?;
    tasks.migrate()?;
    tasks.migrate()?;
    ensure!(tasks.list()?.is_empty());
    println!("PASS actual awman session CRUD and task-store migrations");

    let database = directory.path().join("shared.db");
    let connection = Connection::open(&database)?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE parent (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
         CREATE TABLE child (parent_id INTEGER REFERENCES parent(id));",
    )?;
    let pool = SqlitePoolOptions::new()
        .max_connections(3)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&database)
                .journal_mode(SqliteJournalMode::Wal)
                .foreign_keys(true)
                .busy_timeout(Duration::from_millis(100)),
        )
        .await?;
    let sqlx_version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&pool)
        .await?;
    ensure!(sqlx_version == rusqlite::version());
    let sqlx_source: String = sqlx::query_scalar("SELECT sqlite_source_id()")
        .fetch_one(&pool)
        .await?;
    let rusqlite_source: String =
        connection.query_row("SELECT sqlite_source_id()", [], |row| row.get(0))?;
    ensure!(sqlx_source == rusqlite_source);
    println!("PASS shared SQLite version={sqlx_version} source={sqlx_source}");

    let payload = vec![0_u8, 1, 127, 255];
    connection.execute("INSERT INTO parent VALUES (1, ?1)", [&payload])?;
    let read: Vec<u8> = sqlx::query_scalar("SELECT payload FROM parent WHERE id=1")
        .fetch_one(&pool)
        .await?;
    ensure!(read == payload);
    let inserted: i64 = sqlx::query_scalar("INSERT INTO parent VALUES (2, ?) RETURNING id")
        .bind(&payload)
        .fetch_one(&pool)
        .await?;
    ensure!(inserted == 2);
    let count: i64 = connection.query_row("SELECT count(*) FROM parent", [], |row| row.get(0))?;
    ensure!(count == 2);
    println!("PASS rusqlite/SQLx cross-read/write, BLOB binding and RETURNING");

    let foreign_key_error = sqlx::query("INSERT INTO child VALUES (999)")
        .execute(&pool)
        .await
        .unwrap_err();
    ensure!(foreign_key_error
        .as_database_error()
        .is_some_and(|error| error.is_foreign_key_violation()));
    connection.execute_batch("BEGIN IMMEDIATE; INSERT INTO parent VALUES (3, X'00');")?;
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM parent")
        .fetch_one(&pool)
        .await?;
    ensure!(visible == 2);
    let locked = sqlx::query("INSERT INTO parent VALUES (4, X'00')")
        .execute(&pool)
        .await
        .unwrap_err();
    ensure!(locked
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| code == "5"));
    connection.execute_batch("ROLLBACK;")?;
    sqlx::query("INSERT INTO parent VALUES (4, X'00')")
        .execute(&pool)
        .await?;
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&pool)
        .await?;
    ensure!(integrity == "ok");
    pool.close().await;
    drop(connection);
    let reopened = Connection::open(&database)?;
    let count: i64 = reopened.query_row("SELECT count(*) FROM parent", [], |row| row.get(0))?;
    ensure!(count == 3);
    println!("PASS foreign keys, WAL visibility, SQLITE_BUSY, rollback, reopen and integrity");

    let pools = DbPools::open(
        &directory.path().join("microsandbox.db"),
        3,
        Duration::from_secs(5),
        Duration::from_secs(1),
    )
    .await?;
    Migrator::up(pools.write().inner(), None).await?;
    Migrator::up(pools.write().inner(), None).await?;
    let migrations = Migrator::get_applied_migrations(pools.read().inner()).await?;
    ensure!(migrations.len() == Migrator::migrations().len());
    let result = pools
        .read()
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA integrity_check",
        ))
        .await?
        .expect("integrity result");
    ensure!(result.try_get_by_index::<String>(0)? == "ok");
    println!(
        "PASS actual Microsandbox pools and {} catalog migrations (idempotent)",
        migrations.len()
    );
    Ok(())
}
