//! Real catalog across published migration crates; no VM or host agent.
use microsandbox_migration::{schema_metadata, Migrator, MigratorTrait};
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    let phase = std::env::args().nth(1).unwrap();
    let path = std::env::var("AWMAN_CATALOG_FIXTURE").unwrap();
    let db = Database::connect(format!("sqlite://{path}?mode=rwc"))
        .await
        .unwrap();
    let stmt = |sql: &str| Statement::from_string(DatabaseBackend::Sqlite, sql.to_owned());
    let count = || async {
        db.query_one_raw(stmt("SELECT COUNT(*) FROM seaql_migrations"))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index::<i64>(0)
            .unwrap()
    };
    let rows = || async {
        db.query_all_raw(stmt(
            "SELECT config, active_config FROM sandbox ORDER BY id",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.try_get_by_index::<String>(0).unwrap(),
                row.try_get_by_index::<String>(1).unwrap(),
            )
        })
        .collect::<Vec<_>>()
    };
    match phase.as_str() {
        "write-old" => {
            Migrator::up(&db, None).await.unwrap();
            assert_eq!(count().await, 27);
            for strict_sni in [false, true] {
                let config = json!({
                    "network": {"mode": "none", "strict": true, "strict_sni": strict_sni},
                    "labels": {"awman.test": "preserved"}
                })
                .to_string();
                db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "INSERT INTO sandbox(name, config, active_config, status) VALUES (?, ?, ?, 'stopped')",
                    [format!("awman-{strict_sni}").into(), config.clone().into(), config.into()]
                )).await.unwrap();
            }
        }
        "upgrade-new" => {
            assert_eq!(count().await, 27);
            let before = rows().await;
            // The new secret-config migration validates every row before writes.
            db.execute_unprepared(
                "UPDATE sandbox SET active_config = '{invalid' WHERE name = 'awman-true'",
            )
            .await
            .unwrap();
            let error = Migrator::up(&db, None).await.unwrap_err().to_string();
            assert!(
                error.contains("Database upgrade was not applied"),
                "{error}"
            );
            assert_eq!(count().await, 27);
            assert_eq!(rows().await[0], before[0]);
            db.execute_unprepared("UPDATE sandbox SET active_config = config")
                .await
                .unwrap();
            Migrator::up(&db, None).await.unwrap();
            assert_eq!(count().await, 29);
            for ((config, active), (old, _)) in rows().await.into_iter().zip(before) {
                let new: Value = serde_json::from_str(&config).unwrap();
                let old: Value = serde_json::from_str(&old).unwrap();
                assert_eq!(new["network"]["strict"], true);
                assert_eq!(new["network"]["strict_sni"], old["network"]["strict_sni"]);
                assert_eq!(new["labels"], old["labels"]);
                assert_eq!(serde_json::from_str::<Value>(&active).unwrap(), new);
            }
        }
        "refuse-old" => {
            let before = rows().await;
            let applied = db
                .query_all_raw(stmt("SELECT version FROM seaql_migrations"))
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.try_get_by_index::<String>(0).unwrap())
                .collect::<Vec<_>>();
            // Exact predicate used by 0.7.2 LocalBackend::refuse_schema_ahead.
            assert!(
                schema_metadata::canonical_applied_prefix(applied.iter().map(String::as_str))
                    .is_none(),
                "old reader must refuse a newer schema"
            );
            assert_eq!(count().await, 29);
            assert_eq!(rows().await, before);
        }
        "verify-new" => {
            Migrator::up(&db, None).await.unwrap();
            assert_eq!(count().await, 29);
            assert_eq!(rows().await.len(), 2);
            let integrity = db
                .query_one_raw(stmt("PRAGMA integrity_check"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(integrity.try_get_by_index::<String>(0).unwrap(), "ok");
        }
        _ => panic!("unknown phase"),
    }
    println!("PASS catalog {phase}");
}
