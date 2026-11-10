//! The network listener's generated certificate (WI 0140).
//!
//! SANs are read back by parsing the certificate's DER subjectAltName
//! extension, and cross-checked with rustls' own WebPKI verifier so the
//! names a client would dial really verify.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use awman::data::config::network::TlsSan;
use awman::data::fs::api_paths::ApiPaths;
use awman::data::fs::auth_paths::AuthPathResolver;
use awman::data::fs::daemon_paths::DaemonPaths;
use awman::engine::auth::tls::network_sans;
use awman::engine::auth::{AuthEngine, TlsMaterial};

fn engine(dir: &Path) -> AuthEngine {
    AuthEngine::with_paths(
        AuthPathResolver::at_home(dir),
        ApiPaths::at_root(dir.join("api")),
    )
}

fn unspecified() -> IpAddr {
    "0.0.0.0".parse().unwrap()
}

fn san(s: &str) -> TlsSan {
    TlsSan::parse(s).unwrap()
}

// ─── Minimal DER reader for the subjectAltName extension ─────────────────────

/// Read one DER TLV at `pos`: (tag, content range end, content start).
fn tlv(der: &[u8], pos: usize) -> (u8, usize, usize) {
    let tag = der[pos];
    let first = der[pos + 1] as usize;
    let (len, header) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | der[pos + 2 + i] as usize;
        }
        (len, 2 + n)
    };
    let start = pos + header;
    (tag, start + len, start)
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GeneralName {
    Dns(String),
    Ip(IpAddr),
}

/// The DNS and IP entries of the certificate's subjectAltName extension.
fn parse_sans(cert_pem: &str) -> Vec<GeneralName> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let der = CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap();
    let der: &[u8] = der.as_ref();
    // OID 2.5.29.17 (id-ce-subjectAltName).
    let oid = [0x06, 0x03, 0x55, 0x1d, 0x11];
    let at = der
        .windows(oid.len())
        .position(|w| w == oid)
        .expect("certificate has a subjectAltName extension");
    let mut pos = at + oid.len();
    // Optional `critical BOOLEAN`.
    if der[pos] == 0x01 {
        pos = tlv(der, pos).1;
    }
    let (tag, _, octets) = tlv(der, pos);
    assert_eq!(tag, 0x04, "extnValue is an OCTET STRING");
    let (tag, seq_end, mut pos) = tlv(der, octets);
    assert_eq!(tag, 0x30, "GeneralNames is a SEQUENCE");
    let mut names = Vec::new();
    while pos < seq_end {
        let (tag, end, start) = tlv(der, pos);
        let body = &der[start..end];
        match tag {
            0x82 => names.push(GeneralName::Dns(String::from_utf8(body.to_vec()).unwrap())),
            0x87 => names.push(GeneralName::Ip(match body.len() {
                4 => IpAddr::from(<[u8; 4]>::try_from(body).unwrap()),
                16 => IpAddr::from(<[u8; 16]>::try_from(body).unwrap()),
                n => panic!("iPAddress of {n} bytes"),
            })),
            other => panic!("unexpected GeneralName tag {other:#x}"),
        }
        pos = end;
    }
    names.sort();
    names
}

fn dns(s: &str) -> GeneralName {
    GeneralName::Dns(s.into())
}

fn ip(s: &str) -> GeneralName {
    GeneralName::Ip(s.parse().unwrap())
}

/// Does a WebPKI client that pins this certificate accept it for `name`?
fn verifies_for(material: &TlsMaterial, name: &str) -> bool {
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};
    let cert = CertificateDer::from_pem_slice(material.cert_pem.as_bytes()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()
    .unwrap();
    let server_name = ServerName::try_from(name.to_string()).unwrap();
    verifier
        .verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now())
        .is_ok()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn certificate_covers_loopback_listen_ip_and_tls_sans() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let extra = [san("awman.internal"), san("10.1.2.3"), san("fd00::5")];
    let (identity, regenerated) = engine(tmp.path())
        .ensure_network_tls(&paths, &["192.168.7.9".parse().unwrap()], &extra)
        .unwrap();
    assert!(regenerated, "first call generates");

    let mut expected = vec![
        dns("localhost"),
        dns("awman.internal"),
        ip("127.0.0.1"),
        ip("::1"),
        ip("192.168.7.9"),
        ip("10.1.2.3"),
        ip("fd00::5"),
    ];
    expected.sort();
    assert_eq!(parse_sans(&identity.material.cert_pem), expected);
    assert_eq!(
        identity.sans,
        network_sans(&["192.168.7.9".parse().unwrap()], &extra)
    );

    for name in [
        "localhost",
        "127.0.0.1",
        "::1",
        "awman.internal",
        "10.1.2.3",
        "fd00::5",
        "192.168.7.9",
    ] {
        assert!(verifies_for(&identity.material, name), "{name} must verify");
    }
    assert!(!verifies_for(&identity.material, "other.internal"));
    assert!(!verifies_for(&identity.material, "10.9.9.9"));
}

#[test]
fn unspecified_listen_address_is_not_a_san_but_loopback_always_is() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let (identity, _) = engine(tmp.path())
        .ensure_network_tls(&paths, &["::".parse().unwrap()], &[])
        .unwrap();
    let mut expected = vec![dns("localhost"), ip("127.0.0.1"), ip("::1")];
    expected.sort();
    assert_eq!(parse_sans(&identity.material.cert_pem), expected);
}

#[test]
fn same_sans_reuse_the_certificate() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let auth = engine(tmp.path());
    let extra = [san("awman.internal")];
    let (first, _) = auth
        .ensure_network_tls(&paths, &[unspecified()], &extra)
        .unwrap();
    let (second, regenerated) = auth
        .ensure_network_tls(&paths, &[unspecified()], &extra)
        .unwrap();
    assert!(!regenerated);
    assert_eq!(first, second);
    assert_eq!(auth.load_network_tls(&paths).unwrap(), Some(first));
}

#[test]
fn changed_sans_regenerate_and_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let auth = engine(tmp.path());
    let (before, _) = auth
        .ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();
    assert!(!verifies_for(&before.material, "awman.internal"));

    let (after, regenerated) = auth
        .ensure_network_tls(&paths, &[unspecified()], &[san("awman.internal")])
        .unwrap();
    assert!(regenerated);
    assert_ne!(
        before.material.fingerprint_sha256_hex,
        after.material.fingerprint_sha256_hex
    );
    assert_ne!(before.material.key_pem, after.material.key_pem);
    assert!(parse_sans(&after.material.cert_pem).contains(&dns("awman.internal")));
    assert!(verifies_for(&after.material, "awman.internal"));
    // What is on disk is the new identity.
    assert_eq!(auth.load_network_tls(&paths).unwrap(), Some(after.clone()));
    // The new material builds a server config for a live swap.
    let config = after.material.rustls_server_config().unwrap();
    assert_eq!(
        config.alpn_protocols,
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    );

    // Removing the SAN regenerates again.
    let (_, regenerated) = auth
        .ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();
    assert!(regenerated);
}

#[test]
fn changed_listen_ip_regenerates() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let auth = engine(tmp.path());
    auth.ensure_network_tls(&paths, &["10.0.0.1".parse().unwrap()], &[])
        .unwrap();
    let (identity, regenerated) = auth
        .ensure_network_tls(&paths, &["10.0.0.2".parse().unwrap()], &[])
        .unwrap();
    assert!(regenerated);
    let sans = parse_sans(&identity.material.cert_pem);
    assert!(sans.contains(&ip("10.0.0.2")) && !sans.contains(&ip("10.0.0.1")));
}

#[test]
fn a_missing_san_record_forces_regeneration() {
    // An interrupted regeneration leaves no `sans.json`; never adopt the
    // possibly mismatched key and certificate.
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let auth = engine(tmp.path());
    auth.ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();
    std::fs::remove_file(paths.tls_sans_file()).unwrap();
    assert_eq!(auth.load_network_tls(&paths).unwrap(), None);
    let (_, regenerated) = auth
        .ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();
    assert!(regenerated);
}

#[test]
fn legacy_and_expired_certificates_are_rotated() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let auth = engine(tmp.path());
    let (first, _) = auth
        .ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();

    let record_path = paths.tls_sans_file();
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record.as_object_mut().unwrap().remove("renew_after_unix");
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(auth.load_network_tls(&paths).unwrap(), None);
    let (second, regenerated) = auth
        .ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();
    assert!(regenerated);
    assert_ne!(
        first.material.fingerprint_sha256_hex,
        second.material.fingerprint_sha256_hex
    );

    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record["renew_after_unix"] = serde_json::json!(0);
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(auth.load_network_tls(&paths).unwrap(), None);
    let (third, regenerated) = auth
        .ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();
    assert!(regenerated);
    assert_ne!(
        second.material.fingerprint_sha256_hex,
        third.material.fingerprint_sha256_hex
    );
}

#[test]
fn files_are_owner_only_and_key_is_redacted_in_debug() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = DaemonPaths::new(tmp.path().join("daemon"));
    let (identity, _) = engine(tmp.path())
        .ensure_network_tls(&paths, &[unspecified()], &[])
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [
            paths.tls_key_file(),
            paths.tls_cert_file(),
            paths.tls_sans_file(),
        ] {
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} mode {mode:o}", file.display());
        }
        let dir_mode = std::fs::metadata(paths.tls_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }
    let debug = format!("{identity:?}");
    assert!(
        !debug.contains("PRIVATE KEY"),
        "Debug leaks the private key"
    );
}

#[test]
fn certificate_sans_union_all_tls_listen_addresses_and_extras() {
    let addresses = ["10.0.0.1".parse().unwrap(), "2001:db8::1".parse().unwrap()];
    let sans = network_sans(&addresses, &[san("API.example.org"), san("10.0.0.1")]);
    for expected in [
        "10.0.0.1",
        "2001:db8::1",
        "api.example.org",
        "localhost",
        "127.0.0.1",
        "::1",
    ] {
        assert!(sans.iter().any(|s| s == expected), "{sans:?}");
    }
    assert_eq!(sans.iter().filter(|s| *s == "10.0.0.1").count(), 1);
}
