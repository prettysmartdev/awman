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
