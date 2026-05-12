//! Auth mode type and headless TLS/auth path tests.

use amux::data::fs::headless_paths::HeadlessPaths;

// ─── AuthMode enum ────────────────────────────────────────────────────────────

#[test]
fn auth_mode_types_compile() {
    use amux::frontend::headless::routes::AuthMode;
    let _enabled = AuthMode::Enabled {
        key_hash: "abc123".to_string(),
    };
    let _disabled = AuthMode::Disabled;
}

// ─── API key hash path ────────────────────────────────────────────────────────

#[test]
fn api_key_hash_file_is_under_root() {
    let paths = HeadlessPaths::from_root("/srv/headless");
    let hash = paths.api_key_hash_file();
    assert!(
        hash.starts_with("/srv/headless"),
        "hash file should be under root: {hash:?}"
    );
}

#[test]
fn api_key_hash_filename_is_api_key_hash() {
    let paths = HeadlessPaths::from_root("/srv/headless");
    let hash = paths.api_key_hash_file();
    assert_eq!(hash.file_name().unwrap(), "api_key.hash");
}

// ─── TLS material paths ───────────────────────────────────────────────────────

#[test]
fn tls_cert_file_is_under_tls_dir() {
    let paths = HeadlessPaths::from_root("/srv/headless");
    let cert = paths.tls_cert_file();
    assert!(
        cert.starts_with(paths.tls_dir()),
        "cert file should be under tls dir: {cert:?}"
    );
}

#[test]
fn tls_key_file_is_under_tls_dir() {
    let paths = HeadlessPaths::from_root("/srv/headless");
    let key = paths.tls_key_file();
    assert!(
        key.starts_with(paths.tls_dir()),
        "key file should be under tls dir: {key:?}"
    );
}

#[test]
fn tls_dir_is_under_root() {
    let paths = HeadlessPaths::from_root("/srv/headless");
    assert!(paths.tls_dir().starts_with("/srv/headless"));
}

// ─── PID file ────────────────────────────────────────────────────────────────

#[test]
fn pid_file_is_under_root() {
    let paths = HeadlessPaths::from_root("/srv/headless");
    let pid = paths.pid_file();
    assert!(pid.starts_with("/srv/headless"));
    assert_eq!(pid.file_name().unwrap(), "amux.pid");
}

#[test]
fn log_file_is_under_root() {
    let paths = HeadlessPaths::from_root("/srv/headless");
    let log = paths.log_file();
    assert!(log.starts_with("/srv/headless"));
    assert_eq!(log.file_name().unwrap(), "amux.log");
}
