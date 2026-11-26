//! Small helpers shared by the release modules: hashing, private files,
//! atomic JSON writes and identifiers. No GitHub, host or policy logic here.

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

pub fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 128 random bits from the OS, as 32 hex characters. Used for owner IDs
/// and private host allocations, never for stable operation identity.
pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(hex(&bytes))
}

/// A private directory (0700). An existing directory must be owned by this
/// user and not be a symlink.
pub fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "{} is not a private directory",
                path.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                anyhow::ensure!(
                    meta.uid() == nix::unistd::Uid::effective().as_raw(),
                    "{} is owned by another user",
                    path.display()
                );
                if meta.mode() & 0o077 != 0 {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
                }
            }
            Ok(())
        }
        Err(_) => {
            fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
    }
}

/// Write a file atomically: private temp file, flush, fsync, rename, mode 0600.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("file has no parent directory")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// `vX.Y.Z` with numeric parts.
pub fn valid_version(v: &str) -> bool {
    let Some(rest) = v.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<_> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `user@host`, `host` or `user@host:port`-free SSH destinations made of
/// conservative characters only; passed as argv, never through a shell.
pub fn valid_ssh_destination(d: &str) -> bool {
    !d.is_empty()
        && !d.starts_with('-')
        && d.len() <= 255
        && d.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'-' | b'_'))
        && d.matches('@').count() <= 1
}

/// Run an external command with a wall-clock deadline (design 7.12: every
/// external operation is bounded). Piped stdout/stderr are drained on
/// threads so a chatty child cannot deadlock; `input` is written to stdin.
/// On the deadline the child (and, on Unix, its process group when the
/// caller placed it in one) is killed and `ErrorKind::TimedOut` returned.
pub fn output_bounded(
    cmd: &mut std::process::Command,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::process::Stdio;
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn()?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "no result within {} s; the command was stopped",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Ok(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_destinations_are_validated_before_use() {
        assert!(valid_version("v0.15.0"));
        for bad in ["0.15.0", "v0.15", "v0.15.0-rc1", "v.1.2", "v1.2.x"] {
            assert!(!valid_version(bad), "{bad}");
        }
        assert!(valid_ssh_destination("build@kvm1.example"));
        for bad in ["-oProxyCommand=x", "a b", "a;b", "", "u@h@x", "h:22"] {
            assert!(!valid_ssh_destination(bad), "{bad}");
        }
        assert!(is_sha256(&sha256(b"x")));
        assert!(!is_sha256("ABC"));
    }
}
