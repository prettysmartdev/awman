//! Bounded SSH key metadata inspection. Key material is never retained.

use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::data::error::DataError;

pub const SSH_KEY_HEADER_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshKeyKind {
    Ed25519,
    Rsa,
    Ecdsa,
    Dsa,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKeyInfo {
    pub path: PathBuf,
    pub kind: SshKeyKind,
    pub passphrase_protected: Option<bool>,
    pub mode: Option<u32>,
    pub mode_ok: bool,
    pub public_key_present: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKeyInventory {
    pub ssh_dir: PathBuf,
    pub keys: Vec<SshKeyInfo>,
    pub known_hosts: Option<PathBuf>,
    pub config_present: bool,
    pub warnings: Vec<String>,
}

impl SshKeyInventory {
    pub fn empty(home: &Path) -> Self {
        Self {
            ssh_dir: home.join(".ssh"),
            keys: vec![],
            known_hosts: None,
            config_present: false,
            warnings: vec![],
        }
    }
    pub fn scan(home: &Path) -> Self {
        let mut result = Self::empty(home);
        result.known_hosts =
            Self::validate_key_path(home, &result.ssh_dir.join("known_hosts")).ok();
        result.config_present =
            Self::validate_key_path(home, &result.ssh_dir.join("config")).is_ok();
        let entries = match std::fs::read_dir(&result.ssh_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return result,
            Err(_) => {
                result.warnings.push("cannot read SSH directory".into());
                return result;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                result
                    .warnings
                    .push("cannot inspect SSH directory entry".into());
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("id_") || name.ends_with(".pub") {
                continue;
            }
            let path = match Self::validate_key_path(home, &entry.path()) {
                Ok(path) => path,
                Err(_) => {
                    result
                        .warnings
                        .push(format!("refused SSH key path {}", entry.path().display()));
                    continue;
                }
            };
            let (header, meta) = match read_header(&path, SSH_KEY_HEADER_BYTES) {
                Ok(value) => value,
                Err(_) => {
                    result
                        .warnings
                        .push(format!("cannot read SSH key header {}", path.display()));
                    continue;
                }
            };
            #[cfg(unix)]
            let (mode, mode_ok) = {
                use std::os::unix::fs::MetadataExt;
                let mode = meta.mode() & 0o777;
                (
                    Some(mode),
                    mode & 0o077 == 0 && meta.uid() == nix::unistd::geteuid().as_raw(),
                )
            };
            #[cfg(not(unix))]
            let (mode, mode_ok) = {
                let _ = meta;
                (None, false)
            };
            let public_key_present =
                Self::validate_key_path(home, &entry.path().with_file_name(format!("{name}.pub")))
                    .is_ok();
            result.keys.push(SshKeyInfo {
                kind: key_kind(&header, &name),
                passphrase_protected: detect_passphrase(&header),
                path,
                mode,
                mode_ok,
                public_key_present,
            });
        }
        result.keys.sort_by(|a, b| a.path.cmp(&b.path));
        result
    }
    pub fn validate_key_path(home: &Path, requested: &Path) -> Result<PathBuf, DataError> {
        let requested = if requested == Path::new("~") {
            home.to_path_buf()
        } else if let Ok(tail) = requested.strip_prefix("~/") {
            home.join(tail)
        } else if requested.is_relative() {
            home.join(requested)
        } else {
            requested.to_path_buf()
        };
        let ssh = std::fs::canonicalize(home.join(".ssh"))
            .map_err(|e| DataError::io(home.join(".ssh"), e))?;
        let path = std::fs::canonicalize(&requested).map_err(|e| DataError::io(&requested, e))?;
        if !path.starts_with(&ssh) || !path.is_file() {
            return Err(DataError::InvalidPath {
                path: requested,
                reason: "SSH key must be a regular file inside the canonical ~/.ssh directory"
                    .into(),
            });
        }
        Ok(path)
    }
    pub fn find(&self, path: &Path) -> Option<&SshKeyInfo> {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.keys.iter().find(|key| key.path == path)
    }
    pub fn known_hosts_has(&self, host: &str) -> bool {
        let Some(path) = &self.known_hosts else {
            return false;
        };
        let Ok((bytes, _)) = read_header(path, 1024 * 1024) else {
            return false;
        };
        let text = String::from_utf8_lossy(&bytes);
        text.lines().any(|line| {
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else {
                return false;
            };
            if first.starts_with('#') || first == "@revoked" {
                return false;
            }
            let names = if first.starts_with('@') {
                fields.next().unwrap_or_default()
            } else {
                first
            };
            names.split(',').any(|name| {
                !name.starts_with(['|', '!'])
                    && (name.eq_ignore_ascii_case(host)
                        || name.eq_ignore_ascii_case(&format!("[{host}]:22")))
            })
        })
    }
}

fn read_header(path: &Path, limit: usize) -> Result<(Vec<u8>, std::fs::Metadata), DataError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|e| DataError::io(path, e))?;
    let meta = file.metadata().map_err(|e| DataError::io(path, e))?;
    if !meta.is_file() {
        return Err(DataError::InvalidPath {
            path: path.into(),
            reason: "not a regular file".into(),
        });
    }
    let mut bytes = vec![];
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| DataError::io(path, e))?;
    Ok((bytes, meta))
}

fn openssh_header(header: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(header).ok()?;
    let text = text.strip_prefix("-----BEGIN OPENSSH PRIVATE KEY-----")?;
    let mut encoded: Vec<u8> = text
        .lines()
        .take_while(|l| !l.starts_with("-----END"))
        .flat_map(|l| l.bytes().filter(|b| !b.is_ascii_whitespace()))
        .collect();
    // A bounded read can stop mid-base64 quartet. Only the prefix is needed.
    encoded.truncate(encoded.len() / 4 * 4);
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
}
fn ssh_string<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    *bytes = bytes.get(4..)?;
    let value = bytes.get(..len)?;
    *bytes = bytes.get(len..)?;
    Some(value)
}

pub fn detect_passphrase(header: &[u8]) -> Option<bool> {
    let text = std::str::from_utf8(header).ok()?;
    if text.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----") {
        let decoded = openssh_header(header)?;
        let mut bytes = decoded.strip_prefix(b"openssh-key-v1\0")?;
        ssh_string(&mut bytes)?; // ciphername
        let kdfname = ssh_string(&mut bytes)?;
        return Some(kdfname != b"none");
    }
    if text.starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        return Some(true);
    }
    if ["RSA", "EC", "DSA", ""].iter().any(|kind| {
        text.starts_with(&format!(
            "-----BEGIN {}PRIVATE KEY-----",
            if kind.is_empty() {
                String::new()
            } else {
                format!("{kind} ")
            }
        ))
    }) {
        return Some(
            text.lines()
                .any(|line| line.trim().eq_ignore_ascii_case("Proc-Type: 4,ENCRYPTED")),
        );
    }
    None
}
fn key_kind(header: &[u8], name: &str) -> SshKeyKind {
    let text = String::from_utf8_lossy(header);
    if let Some(decoded) = openssh_header(header) {
        let algorithm = (|| {
            let mut bytes = decoded.strip_prefix(b"openssh-key-v1\0")?;
            ssh_string(&mut bytes)?;
            ssh_string(&mut bytes)?;
            ssh_string(&mut bytes)?;
            bytes = bytes.get(4..)?; // key count
            let mut public = ssh_string(&mut bytes)?;
            ssh_string(&mut public)
        })();
        match algorithm {
            Some(b"ssh-ed25519") => return SshKeyKind::Ed25519,
            Some(b"ssh-rsa") => return SshKeyKind::Rsa,
            Some(b"ssh-dss") => return SshKeyKind::Dsa,
            Some(a) if a.starts_with(b"ecdsa-") => return SshKeyKind::Ecdsa,
            _ => {}
        }
    }
    if text.contains("BEGIN RSA PRIVATE KEY") || name == "id_rsa" {
        SshKeyKind::Rsa
    } else if text.contains("BEGIN EC PRIVATE KEY") || name == "id_ecdsa" {
        SshKeyKind::Ecdsa
    } else if text.contains("BEGIN DSA PRIVATE KEY") || name == "id_dsa" {
        SshKeyKind::Dsa
    } else if name == "id_ed25519" {
        SshKeyKind::Ed25519
    } else {
        SshKeyKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_and_openssh_encryption_headers() {
        assert_eq!(
            detect_passphrase(b"-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n"),
            Some(true)
        );
        assert_eq!(
            detect_passphrase(b"-----BEGIN PRIVATE KEY-----\n"),
            Some(false)
        );
        assert_eq!(detect_passphrase(b"not a private key"), None);
        for kdf in [b"none".as_slice(), b"bcrypt"] {
            let mut bytes = b"openssh-key-v1\0".to_vec();
            for field in [b"none".as_slice(), kdf] {
                bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
                bytes.extend_from_slice(field);
            }
            let text = format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            assert_eq!(detect_passphrase(text.as_bytes()), Some(kdf != b"none"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_refuses_escape_checks_mode_and_only_stores_metadata() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let key = ssh.join("id_rsa");
        std::fs::write(&key, b"-----BEGIN RSA PRIVATE KEY-----\n").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let outside = home.path().join("outside");
        std::fs::write(&outside, b"not a key").unwrap();
        symlink(&outside, ssh.join("id_escape")).unwrap();
        std::fs::write(
            ssh.join("known_hosts"),
            "ghe.example ssh-ed25519 public\n|1|hashed|host ssh-ed25519 public\n",
        )
        .unwrap();
        let inventory = SshKeyInventory::scan(home.path());
        assert_eq!(inventory.keys.len(), 1);
        assert!(inventory.keys[0].mode_ok);
        assert!(inventory.known_hosts_has("ghe.example"));
        assert!(!inventory.known_hosts_has("github.com"));
        assert!(SshKeyInventory::validate_key_path(home.path(), &ssh.join("id_escape")).is_err());
    }
}

#[cfg(test)]
mod header_tests {
    use super::*;

    fn openssh(kdf: &[u8], cipher: &[u8], algorithm: &[u8]) -> String {
        let mut bytes = b"openssh-key-v1\0".to_vec();
        for field in [cipher, kdf] {
            bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bytes.extend_from_slice(field);
        }
        // kdf options (empty), key count, public key blob
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        let mut blob = Vec::new();
        blob.extend_from_slice(&(algorithm.len() as u32).to_be_bytes());
        blob.extend_from_slice(algorithm);
        bytes.extend_from_slice(&(blob.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&blob);
        format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    #[test]
    fn detect_passphrase_openssh_none_is_unencrypted() {
        let key = openssh(b"none", b"none", b"ssh-ed25519");
        assert_eq!(detect_passphrase(key.as_bytes()), Some(false));
        assert_eq!(key_kind(key.as_bytes(), "whatever"), SshKeyKind::Ed25519);
    }

    #[test]
    fn detect_passphrase_openssh_bcrypt_is_encrypted() {
        let key = openssh(b"bcrypt", b"aes256-ctr", b"ssh-rsa");
        assert_eq!(detect_passphrase(key.as_bytes()), Some(true));
        assert_eq!(key_kind(key.as_bytes(), "whatever"), SshKeyKind::Rsa);
    }

    #[test]
    fn detect_passphrase_pem_encrypted_and_pkcs8_encrypted() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,AA\n\nabc\n";
        assert_eq!(detect_passphrase(pem.as_bytes()), Some(true));
        let plain = "-----BEGIN EC PRIVATE KEY-----\nMHcCAQ\n-----END EC PRIVATE KEY-----\n";
        assert_eq!(detect_passphrase(plain.as_bytes()), Some(false));
        assert_eq!(
            detect_passphrase(b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nabc"),
            Some(true)
        );
    }

    #[test]
    fn detect_passphrase_unknown_or_truncated_headers_are_none() {
        assert_eq!(detect_passphrase(b""), None);
        assert_eq!(detect_passphrase(b"ssh-ed25519 AAAA public"), None);
        assert_eq!(detect_passphrase(&[0xff, 0xfe, 0x00]), None);
        // Truncated OpenSSH body: not enough bytes to read the kdf name.
        assert_eq!(
            detect_passphrase(b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn\n"),
            None
        );
    }

    #[cfg(unix)]
    fn write_key(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_pub_files_and_non_id_files_and_classifies_each_kind() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        write_key(
            &ssh,
            "id_ed25519",
            &openssh(b"none", b"none", b"ssh-ed25519"),
            0o600,
        );
        write_key(&ssh, "id_ed25519.pub", "ssh-ed25519 AAAA", 0o644);
        write_key(
            &ssh,
            "id_enc",
            &openssh(b"bcrypt", b"aes256-ctr", b"ssh-ed25519"),
            0o600,
        );
        write_key(&ssh, "config", "Host *\n", 0o600);
        write_key(&ssh, "authorized_keys", "ssh-rsa AAAA", 0o600);
        write_key(
            &ssh,
            "id_pem",
            "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n",
            0o600,
        );
        let inventory = SshKeyInventory::scan(home.path());
        let names: Vec<_> = inventory
            .keys
            .iter()
            .map(|k| k.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["id_ed25519", "id_enc", "id_pem"]);
        let by_name = |n: &str| {
            inventory
                .keys
                .iter()
                .find(|k| k.path.file_name().unwrap() == n)
                .unwrap()
        };
        assert_eq!(by_name("id_ed25519").passphrase_protected, Some(false));
        assert!(by_name("id_ed25519").public_key_present);
        assert_eq!(by_name("id_enc").passphrase_protected, Some(true));
        assert_eq!(by_name("id_pem").passphrase_protected, Some(true));
        assert!(!by_name("id_enc").public_key_present);
        assert!(inventory.config_present);
        assert!(inventory.warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_without_ssh_dir_is_empty_and_without_warnings() {
        let home = tempfile::tempdir().unwrap();
        let inventory = SshKeyInventory::scan(home.path());
        assert!(inventory.keys.is_empty());
        assert!(inventory.warnings.is_empty());
        assert!(inventory.known_hosts.is_none());
        assert!(!inventory.config_present);
    }

    #[cfg(unix)]
    #[test]
    fn scan_reads_only_the_bounded_header() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        // A huge "key": the header limit means this is read in constant space.
        let mut body = String::from("-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n");
        body.push_str(&"A".repeat(5 * 1024 * 1024));
        write_key(&ssh, "id_big", &body, 0o600);
        let inventory = SshKeyInventory::scan(home.path());
        assert_eq!(inventory.keys.len(), 1);
        assert_eq!(inventory.keys[0].passphrase_protected, Some(true));
        let (bytes, _) = read_header(&inventory.keys[0].path, SSH_KEY_HEADER_BYTES).unwrap();
        assert_eq!(bytes.len(), SSH_KEY_HEADER_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_key_is_a_warning_not_a_key() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let key = write_key(&ssh, "id_locked", "-----BEGIN PRIVATE KEY-----\n", 0o000);
        // Root ignores mode bits; only assert when the permission is enforced.
        if std::fs::File::open(&key).is_ok() {
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
            return;
        }
        let inventory = SshKeyInventory::scan(home.path());
        assert!(inventory.keys.is_empty());
        assert!(inventory.warnings.iter().any(|w| w.contains("id_locked")));
    }

    #[cfg(unix)]
    #[test]
    fn mode_ok_false_for_0644_and_group_readable() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        write_key(&ssh, "id_open", "-----BEGIN PRIVATE KEY-----\n", 0o644);
        write_key(&ssh, "id_group", "-----BEGIN PRIVATE KEY-----\n", 0o640);
        write_key(&ssh, "id_ok", "-----BEGIN PRIVATE KEY-----\n", 0o400);
        let inventory = SshKeyInventory::scan(home.path());
        let get = |n: &str| inventory.keys.iter().find(|k| k.path.ends_with(n)).unwrap();
        assert!(!get("id_open").mode_ok);
        assert_eq!(get("id_open").mode, Some(0o644));
        assert!(!get("id_group").mode_ok);
        assert!(get("id_ok").mode_ok);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused_by_scan_and_validation() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let outside = home.path().join("elsewhere");
        std::fs::write(&outside, "-----BEGIN PRIVATE KEY-----\n").unwrap();
        symlink(&outside, ssh.join("id_link")).unwrap();
        // A symlink to a directory outside ~/.ssh is refused too.
        let other = home.path().join("otherdir");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("id_inner"), "x").unwrap();
        symlink(&other, ssh.join("sub")).unwrap();
        let inventory = SshKeyInventory::scan(home.path());
        assert!(inventory.keys.is_empty());
        assert!(inventory.warnings.iter().any(|w| w.contains("id_link")));
        for requested in [
            ssh.join("id_link"),
            ssh.join("sub/id_inner"),
            outside.clone(),
        ] {
            assert!(
                SshKeyInventory::validate_key_path(home.path(), &requested).is_err(),
                "{requested:?}"
            );
        }
        // A symlink that stays inside ~/.ssh is fine (canonical target is inside).
        write_key(&ssh, "real", "k", 0o600);
        symlink(ssh.join("real"), ssh.join("id_inside")).unwrap();
        let ok = SshKeyInventory::validate_key_path(home.path(), &ssh.join("id_inside")).unwrap();
        assert_eq!(ok, std::fs::canonicalize(ssh.join("real")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn validate_key_path_refuses_directories_dotdot_and_missing_and_expands_tilde() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        write_key(&ssh, "id_a", "k", 0o600);
        std::fs::write(home.path().join("secret"), "s").unwrap();
        assert!(SshKeyInventory::validate_key_path(home.path(), &ssh).is_err());
        assert!(SshKeyInventory::validate_key_path(home.path(), &ssh.join("missing")).is_err());
        assert!(SshKeyInventory::validate_key_path(home.path(), &ssh.join("../secret")).is_err());
        let tilde =
            SshKeyInventory::validate_key_path(home.path(), Path::new("~/.ssh/id_a")).unwrap();
        assert!(tilde.ends_with(".ssh/id_a"));
        let relative =
            SshKeyInventory::validate_key_path(home.path(), Path::new(".ssh/id_a")).unwrap();
        assert_eq!(relative, tilde);
        // No ~/.ssh at all.
        let bare = tempfile::tempdir().unwrap();
        assert!(SshKeyInventory::validate_key_path(bare.path(), Path::new("~/.ssh/id_a")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_header_refuses_symlinks_and_fifos() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, "x").unwrap();
        symlink(&real, dir.path().join("link")).unwrap();
        assert!(read_header(&dir.path().join("link"), 16).is_err());
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert!(read_header(&fifo, 16).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn known_hosts_matching_handles_ports_markers_comments_and_hashes() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(
            ssh.join("known_hosts"),
            "# github.com ssh-ed25519 comment\n\
             @revoked revoked.example ssh-ed25519 k\n\
             @cert-authority ca.example ssh-ed25519 k\n\
             other.example,Alt.Example ssh-ed25519 k\n\
             [bracket.example]:22 ssh-ed25519 k\n\
             |1|abc|def ssh-ed25519 k\n\
             !negated.example ssh-ed25519 k\n",
        )
        .unwrap();
        let inventory = SshKeyInventory::scan(home.path());
        assert!(inventory.known_hosts_has("ca.example"));
        assert!(inventory.known_hosts_has("alt.example"));
        assert!(inventory.known_hosts_has("OTHER.example"));
        assert!(inventory.known_hosts_has("bracket.example"));
        assert!(!inventory.known_hosts_has("github.com"));
        assert!(!inventory.known_hosts_has("revoked.example"));
        assert!(!inventory.known_hosts_has("negated.example"));
        assert!(!SshKeyInventory::empty(home.path()).known_hosts_has("x"));
    }

    #[test]
    fn inventory_serializes_metadata_only() {
        let mut inventory = SshKeyInventory::empty(Path::new("/h"));
        inventory.keys.push(SshKeyInfo {
            path: "/h/.ssh/id_x".into(),
            kind: SshKeyKind::Ed25519,
            passphrase_protected: Some(false),
            mode: Some(0o600),
            mode_ok: true,
            public_key_present: false,
        });
        let json = serde_json::to_string(&inventory).unwrap();
        assert!(!json.contains("PRIVATE KEY"));
        assert!(json.contains("ed25519"));
    }
}
