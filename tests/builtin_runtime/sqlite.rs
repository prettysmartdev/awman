//! One native SQLite in the whole graph, and the SDK catalog and awman's own
//! stores coexisting on it. Re-run whenever SQLx, rusqlite or the bundled
//! SQLite change (see tools/oci-runtime-spike/sqlite-resolution/README.md).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use crate::binary::{awman, has_builtin_runtime, Scratch};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

struct Package {
    name: String,
    version: String,
    source: Option<String>,
}

fn lock_packages() -> Vec<Package> {
    let lock: toml::Value =
        toml::from_str(&std::fs::read_to_string(root().join("Cargo.lock")).unwrap()).unwrap();
    lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| Package {
            name: p["name"].as_str().unwrap().to_owned(),
            version: p["version"].as_str().unwrap().to_owned(),
            source: p.get("source").and_then(|s| s.as_str()).map(str::to_owned),
        })
        .collect()
}

fn only<'a>(packages: &'a [Package], name: &str) -> &'a Package {
    let found: Vec<_> = packages.iter().filter(|p| p.name == name).collect();
    assert_eq!(
        found.len(),
        1,
        "{name}: expected exactly one version in Cargo.lock, found {:?}",
        found.iter().map(|p| &p.version).collect::<Vec<_>>()
    );
    found[0]
}

#[test]
fn builtin_sqlite_exactly_one_native_binding_in_the_dependency_graph() {
    let packages = lock_packages();
    // Two `links = "sqlite3"` crates cannot link into one executable; a second
    // libsqlite3-sys (or a system libsqlite3 binding) means the patch regressed.
    let native = only(&packages, "libsqlite3-sys");
    assert!(native.version.starts_with("0.38."), "{}", native.version);
    assert!(
        packages
            .iter()
            .filter(|p| p.name.contains("sqlite3-sys"))
            .count()
            == 1,
        "no other sqlite3 -sys crate may appear"
    );
    let rusqlite = only(&packages, "rusqlite");
    assert!(
        rusqlite.version.starts_with("0.40."),
        "awman's rusqlite must stay unchanged: {}",
        rusqlite.version
    );
    assert!(
        packages
            .iter()
            .all(|p| p.name != "libdbus-sys" && p.name != "keyring"),
        "the SDK's keyring/dbus feature must stay off"
    );
}

#[test]
fn builtin_sqlite_sqlx_is_the_vendored_patched_crate_and_the_patch_is_documented() {
    let packages = lock_packages();
    let sqlx = only(&packages, "sqlx-sqlite");
    assert_eq!(sqlx.version, "0.9.0");
    assert!(
        sqlx.source.is_none(),
        "sqlx-sqlite must resolve to the vendored path crate, not the registry: {:?}",
        sqlx.source
    );
    let vendored = root().join("third_party/sqlx-sqlite-0.9.0");
    let manifest = std::fs::read_to_string(vendored.join("Cargo.toml")).unwrap();
    let section = manifest
        .split("[dependencies.libsqlite3-sys]")
        .nth(1)
        .expect("libsqlite3-sys dependency section");
    assert!(
        section.lines().take(6).any(|l| l.contains("<0.39.0")),
        "the vendored bound must admit libsqlite3-sys 0.38"
    );
    assert!(!section.lines().take(6).any(|l| l.contains("<0.38.0")));
    for file in ["PATCH.md", "upstream.diff"] {
        assert!(
            vendored.join(file).is_file(),
            "{file} documents the removal path"
        );
    }
    let root_manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    assert!(
        root_manifest.contains("sqlx-sqlite")
            && root_manifest.contains("third_party/sqlx-sqlite-0.9.0")
    );
}

#[test]
fn builtin_sqlite_bundled_library_version_is_what_rusqlite_reports() {
    // Both the SDK's SQLx driver and awman's rusqlite use this one library.
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    let version: String = connection
        .query_row("select sqlite_version()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, rusqlite::version());
    assert!(version.starts_with('3'));
}

// ─── SDK catalog <-> rusqlite, through the real binary ──────────────────────

fn short_state() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("awb.")
        .tempdir_in("/tmp")
        .unwrap()
}

pub(crate) fn config_home(scratch: &Scratch) -> PathBuf {
    let home = scratch.home();
    // Scratch::command points XDG_CONFIG_HOME at home/.config; awman keeps its global
    // config in $XDG_CONFIG_HOME/awman when that is set.
    let data = home.join(".config/awman");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("config.json"), br#"{"runtime":"builtin"}"#).unwrap();
    home
}

/// `awman status` under the opt-in gate: constructs the builtin runtime (which
/// opens and migrates the SDK catalog through SQLx) and lists agents. It keeps
/// refreshing, so it is killed once the listing appeared - exactly like a crash.
pub(crate) fn status_until_listed(scratch: &Scratch, state: &Path) -> Result<String, String> {
    let repo = scratch.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut child: Child = scratch
        .command(&awman(), Some("/usr/bin:/bin"))
        .env("AWMAN_TEST_BUILTIN", "1")
        .env("AWMAN_BUILTIN_STATE_DIR", state)
        .arg("status")
        .current_dir(&repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    for stream in [
        Box::new(stdout) as Box<dyn std::io::Read + Send>,
        Box::new(stderr),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut seen = String::new();
    let outcome = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                seen.push_str(&line);
                seen.push('\n');
                if line.contains("No code agents running") {
                    break Ok(seen.clone());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(seen.clone()),
            Err(_) if Instant::now() > deadline => break Err(format!("timed out; saw: {seen}")),
            Err(_) => {}
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

fn tables(db: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(db).unwrap();
    let mut statement = connection
        .prepare("select name from sqlite_master where type = 'table' order by name")
        .unwrap();
    statement
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn applied_migrations(db: &Path) -> i64 {
    let connection = rusqlite::Connection::open(db).unwrap();
    let table = tables(db)
        .into_iter()
        .find(|t| t.contains("migration") && !t.contains("lock"))
        .expect("a migrations table");
    connection
        .query_row(&format!("select count(*) from \"{table}\""), [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn builtin_sqlite_sdk_catalog_migrates_twice_and_survives_cross_driver_writes_and_a_crash() {
    if !has_builtin_runtime() {
        eprintln!(
            "SKIP: builtin_sqlite catalog round trip: awman was built without the builtin runtime"
        );
        return;
    }
    let state = short_state();
    let state_dir = state.path().join("s");
    let scratch = Scratch::new();
    config_home(&scratch);

    // First start: SQLx creates and migrates the catalog (then the process is
    // killed, leaving a WAL and lock files behind like a crash would).
    let first = status_until_listed(&scratch, &state_dir).unwrap();
    assert!(
        !first.contains("unavailable on this host"),
        "builtin must have opened, not fallen back: {first}"
    );
    let db = state_dir.join("db/msb.db");
    assert!(
        db.is_file(),
        "the SDK catalog must exist; state dir: {:?}; output: {first}",
        std::fs::read_dir(&state_dir)
            .map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
    );
    let first_tables = tables(&db);
    assert!(first_tables.len() > 5, "{first_tables:?}");
    let migrations = applied_migrations(&db);
    assert!(migrations >= 1, "migrations must have been recorded");

    // rusqlite (awman's stack, same native library) commits a cross-driver transaction.
    {
        let mut connection = rusqlite::Connection::open(&db).unwrap();
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute(
            "create table awman_cross_driver_probe (id integer primary key, v text not null)",
            [],
        )
        .unwrap();
        tx.execute(
            "insert into awman_cross_driver_probe (v) values ('written by rusqlite')",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Second start after the "crash": stale locks/WAL are recovered, migrations
    // are idempotent, and SQLx neither drops nor rewrites rusqlite's table.
    let second = status_until_listed(&scratch, &state_dir).unwrap();
    assert!(!second.contains("unavailable on this host"), "{second}");
    assert_eq!(
        applied_migrations(&db),
        migrations,
        "re-running the migrations must be a no-op"
    );
    let second_tables = tables(&db);
    for table in &first_tables {
        assert!(second_tables.contains(table), "{table} vanished");
    }
    let connection = rusqlite::Connection::open(&db).unwrap();
    let value: String = connection
        .query_row("select v from awman_cross_driver_probe", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "written by rusqlite");
    let integrity: String = connection
        .query_row("pragma integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

#[test]
fn builtin_sqlite_an_unversioned_foreign_catalog_is_refused_not_adopted_or_migrated() {
    if !has_builtin_runtime() {
        eprintln!(
            "SKIP: builtin_sqlite foreign catalog: awman was built without the builtin runtime"
        );
        return;
    }
    let state = short_state();
    let state_dir = state.path().join("s");
    let scratch = Scratch::new();
    config_home(&scratch);
    // A previous, unversioned catalog (an "old" one) sits where the SDK's would.
    std::fs::create_dir_all(state_dir.join("db")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(state_dir.join("db"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let old = rusqlite::Connection::open(state_dir.join("db/msb.db")).unwrap();
        old.execute("create table old_layout (id integer primary key)", [])
            .unwrap();
        old.execute("insert into old_layout default values", [])
            .unwrap();
    }
    // `status` needs no runtime, but a configured builtin runtime that cannot open its catalog
    // is a hard refusal (never a silent switch to another runtime that would ignore the agents).
    let seen =
        status_until_listed(&scratch, &state_dir).expect_err("the foreign catalog must be refused");
    assert!(
        seen.contains("protocol mismatch") && seen.contains("unversioned"),
        "the refusal must say why: {seen}"
    );
    assert!(
        !state_dir.join("awman-runtime-version").exists(),
        "nothing is stamped onto a foreign catalog"
    );
    assert_eq!(
        tables(&state_dir.join("db/msb.db")),
        ["old_layout"],
        "the old catalog is not migrated"
    );
}
