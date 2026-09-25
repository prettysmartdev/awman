//! Content verification primitives shared by every image source: SHA-256
//! hashing, byte caps, free-space checks and the staging sink every adapter
//! writes through.
//!
//! Nothing here knows about a particular source. Each adapter streams bytes
//! into a [`StagingSink`], which refuses to grow past the archive cap or to
//! eat into the free space the cache must leave behind, so an oversized or
//! runaway transfer is stopped while it is still in the private staging
//! directory — before anything is visible in the cache.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::data::oci_identity::Digest;
use crate::engine::error::EngineError;

/// Copy buffer size for every streaming loop.
pub(super) const CHUNK: usize = 64 * 1024;

/// How often (in bytes written) a sink re-checks free space.
const DISK_CHECK_INTERVAL: u64 = 16 * 1024 * 1024;

/// Hex SHA-256 of `bytes`.
pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// `sha256:<hex>` digest of `bytes`.
pub(super) fn sha256_digest(bytes: &[u8]) -> Digest {
    digest_from_hex(&sha256_hex(bytes))
}

/// A [`Digest`] from 64 lowercase hex characters produced by [`Sha256`].
pub(super) fn digest_from_hex(hex: &str) -> Digest {
    // Our own hasher's output is always valid; a failure here is a bug.
    Digest::parse(&format!("sha256:{hex}")).expect("sha256 output is a valid digest")
}

/// The hex part of a digest.
pub(super) fn digest_hex(digest: &Digest) -> &str {
    digest.as_str().trim_start_matches("sha256:")
}

pub(super) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// A reader that hashes and counts what passes through it, and fails once
/// more than `limit` bytes have been read (a decompression bomb or an
/// oversized blob stops here instead of filling the disk or memory).
pub(super) struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    count: u64,
    limit: u64,
}

impl<R: Read> HashingReader<R> {
    pub(super) fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            count: 0,
            limit,
        }
    }

    /// Bytes read so far.
    pub(super) fn count(&self) -> u64 {
        self.count
    }

    /// Read to the end (so the hash covers the whole stream) and return
    /// `(hex digest, byte count)`.
    pub(super) fn finish(mut self) -> io::Result<(String, u64)> {
        io::copy(&mut self, &mut io::sink())?;
        Ok((hex(&self.hasher.finalize()), self.count))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        if self.count > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                LimitExceeded(self.limit),
            ));
        }
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// The error a [`HashingReader`] raises when its cap is exceeded, so callers
/// can tell "too big" from "corrupt".
#[derive(Debug)]
pub(super) struct LimitExceeded(pub(super) u64);

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exceeds the {}-byte limit", self.0)
    }
}

impl std::error::Error for LimitExceeded {}

/// Whether `err` (or its source) is a [`LimitExceeded`].
pub(super) fn is_limit_exceeded(err: &io::Error) -> bool {
    err.get_ref()
        .is_some_and(|inner| inner.downcast_ref::<LimitExceeded>().is_some())
}

/// Free-space probe. Injectable so the "insufficient disk space" path is
/// testable without filling a disk.
pub trait DiskSpace: Send + Sync {
    /// Bytes available to this user on the filesystem holding `path`, or
    /// `None` when the platform cannot tell.
    fn available(&self, path: &Path) -> Option<u64>;
}

/// The real filesystem.
pub(super) struct HostDiskSpace;

impl DiskSpace for HostDiskSpace {
    #[cfg(unix)]
    fn available(&self, path: &Path) -> Option<u64> {
        let stat = nix::sys::statvfs::statvfs(path).ok()?;
        // Available to unprivileged users, not the root-reserved total.
        Some((stat.blocks_available() as u64).saturating_mul(stat.fragment_size() as u64))
    }

    #[cfg(not(unix))]
    fn available(&self, _path: &Path) -> Option<u64> {
        None
    }
}

/// Fail unless `needed` more bytes fit on `path`'s filesystem while leaving
/// `min_free` untouched.
pub(super) fn ensure_space(
    disk: &dyn DiskSpace,
    path: &Path,
    needed: u64,
    min_free: u64,
) -> Result<(), EngineError> {
    let Some(available) = disk.available(path) else {
        return Ok(());
    };
    let required = needed.saturating_add(min_free);
    if available < required {
        return Err(EngineError::InsufficientDiskSpace {
            path: path.to_path_buf(),
            needed: required,
            available,
        });
    }
    Ok(())
}

/// A file in the private staging directory that an adapter streams an
/// archive (or a blob) into. Enforces the byte cap and the free-space floor
/// as it grows and remembers the typed reason it stopped, so an adapter that
/// only sees an `io::Error` from a copy loop can still report
/// `InsufficientDiskSpace` or `ImageArchiveRejected` precisely.
pub(super) struct StagingSink<'a> {
    path: PathBuf,
    file: File,
    written: u64,
    since_check: u64,
    max_bytes: u64,
    min_free: u64,
    disk: &'a dyn DiskSpace,
    failure: Option<EngineError>,
}

impl<'a> StagingSink<'a> {
    pub(super) fn create(
        path: PathBuf,
        max_bytes: u64,
        min_free: u64,
        disk: &'a dyn DiskSpace,
    ) -> Result<Self, EngineError> {
        let file = create_private_file(&path)?;
        Ok(Self {
            path,
            file,
            written: 0,
            // Probe before the very first write, not only after 16 MiB.
            since_check: DISK_CHECK_INTERVAL,
            max_bytes,
            min_free,
            disk,
            failure: None,
        })
    }

    pub(super) fn written(&self) -> u64 {
        self.written
    }

    /// Append `chunk`, enforcing the cap and the free-space floor.
    pub(super) fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), EngineError> {
        let next = self.written + chunk.len() as u64;
        if next > self.max_bytes {
            return Err(EngineError::ImageArchiveRejected {
                path: self.path.clone(),
                reason: format!("archive exceeds the {}-byte limit", self.max_bytes),
            });
        }
        if self.since_check.saturating_add(chunk.len() as u64) > DISK_CHECK_INTERVAL {
            self.since_check = 0;
            // Reserve the whole window until the next probe (bounded by
            // the cap), so the free-space floor cannot be crossed between
            // checks.
            let window = (self.max_bytes - self.written)
                .min(DISK_CHECK_INTERVAL)
                .max(chunk.len() as u64);
            let dir = self.path.parent().unwrap_or(Path::new("."));
            ensure_space(self.disk, dir, window, self.min_free)?;
        }
        self.since_check += chunk.len() as u64;
        self.file
            .write_all(chunk)
            .map_err(|e| EngineError::io(self.path.clone(), e))?;
        self.written = next;
        Ok(())
    }

    /// Stream everything from `reader` into the sink. A read error is a
    /// truncated or failed transfer.
    pub(super) fn copy_from(&mut self, reader: &mut dyn Read) -> Result<u64, EngineError> {
        let mut buf = vec![0u8; CHUNK];
        let start = self.written;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(if is_limit_exceeded(&e) {
                        EngineError::ImageArchiveRejected {
                            path: self.path.clone(),
                            reason: format!("decompressed archive {e}"),
                        }
                    } else {
                        EngineError::Network(format!(
                            "transfer into {} failed after {} bytes: {e}",
                            self.path.display(),
                            self.written - start
                        ))
                    })
                }
            };
            self.write_chunk(&buf[..n])?;
        }
        Ok(self.written - start)
    }

    /// Flush and fsync, returning the finished file's path and size.
    pub(super) fn finish(mut self) -> Result<(PathBuf, u64), EngineError> {
        self.file
            .flush()
            .and_then(|_| self.file.sync_all())
            .map_err(|e| EngineError::io(self.path.clone(), e))?;
        Ok((self.path, self.written))
    }

    /// The typed failure recorded by the `io::Write` impl, if any.
    pub(super) fn take_failure(&mut self) -> Option<EngineError> {
        self.failure.take()
    }
}

/// `io::Write` for code that needs a writer (the `tar` builder). A cap or
/// free-space failure is recorded and surfaced through
/// [`StagingSink::take_failure`].
impl Write for StagingSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.write_chunk(buf) {
            Ok(()) => Ok(buf.len()),
            Err(e) => {
                let msg = e.to_string();
                self.failure = Some(e);
                Err(io::Error::other(msg))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Create a new file readable and writable only by this user. Fails if it
/// already exists, so a staging name can never be pre-planted.
pub(super) fn create_private_file(path: &Path) -> Result<File, EngineError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|e| EngineError::io(path, e))
}

/// Create `dir` (and parents) and restrict it to this user. A pre-existing
/// `dir` must be a real directory owned by this user: a symlink (or someone
/// else's directory) is refused rather than followed and re-permissioned.
pub(super) fn create_private_dir(dir: &Path) -> Result<(), EngineError> {
    std::fs::create_dir_all(dir).map_err(|e| EngineError::io(dir, e))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| EngineError::io(dir, e))?;
    if !meta.file_type().is_dir() {
        return Err(EngineError::Config(format!(
            "image cache directory {} is not a plain directory (symlinks are refused)",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(EngineError::Config(format!(
                "image cache directory {} is owned by another user",
                dir.display()
            )));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| EngineError::io(dir, e))?;
    }
    Ok(())
}

/// Hash a whole file: `(hex, size)`.
pub(super) fn hash_file(path: &Path) -> Result<(String, u64), EngineError> {
    let file = File::open(path).map_err(|e| EngineError::io(path, e))?;
    HashingReader::new(file, u64::MAX)
        .finish()
        .map_err(|e| EngineError::io(path, e))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A probe that reports a fixed amount of free space.
    pub(crate) struct FixedDisk(pub(crate) Option<u64>);

    impl DiskSpace for FixedDisk {
        fn available(&self, _path: &Path) -> Option<u64> {
            self.0
        }
    }

    #[test]
    fn sha256_of_empty_input_is_the_well_known_value() {
        assert_eq!(
            sha256_digest(b"").as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hashing_reader_counts_hashes_and_caps() {
        let (hex, n) = HashingReader::new(&b"abc"[..], 3).finish().unwrap();
        assert_eq!(n, 3);
        assert_eq!(hex, sha256_hex(b"abc"));

        let err = HashingReader::new(&b"abcd"[..], 3).finish().unwrap_err();
        assert!(is_limit_exceeded(&err), "{err}");
    }

    #[test]
    fn ensure_space_reports_needed_including_the_floor() {
        let disk = FixedDisk(Some(100));
        assert!(ensure_space(&disk, Path::new("/x"), 50, 50).is_ok());
        match ensure_space(&disk, Path::new("/x"), 51, 50) {
            Err(EngineError::InsufficientDiskSpace {
                needed, available, ..
            }) => {
                assert_eq!(needed, 101);
                assert_eq!(available, 100);
            }
            other => panic!("expected InsufficientDiskSpace, got {other:?}"),
        }
        // Unknown free space never blocks.
        assert!(ensure_space(&FixedDisk(None), Path::new("/x"), u64::MAX, 1).is_ok());
    }

    #[test]
    fn staging_sink_rejects_growth_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let disk = FixedDisk(None);
        let mut sink = StagingSink::create(dir.path().join("s"), 4, 0, &disk).unwrap();
        sink.write_chunk(b"abcd").unwrap();
        assert!(matches!(
            sink.write_chunk(b"e"),
            Err(EngineError::ImageArchiveRejected { .. })
        ));
        assert_eq!(sink.written(), 4);
    }

    #[test]
    fn staging_sink_stops_when_the_disk_fills() {
        let dir = tempfile::tempdir().unwrap();
        let disk = FixedDisk(Some(10));
        let mut sink = StagingSink::create(dir.path().join("s"), u64::MAX, 1024, &disk).unwrap();
        let chunk = vec![0u8; DISK_CHECK_INTERVAL as usize];
        assert!(matches!(
            sink.write_chunk(&chunk),
            Err(EngineError::InsufficientDiskSpace { .. })
        ));
    }

    #[test]
    fn staging_file_cannot_be_preplanted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        std::fs::write(&path, b"planted").unwrap();
        assert!(StagingSink::create(path, 1, 0, &FixedDisk(None)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_dirs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a/b");
        create_private_dir(&sub).unwrap();
        let mode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        create_private_file(&sub.join("f")).unwrap();
        let mode = std::fs::metadata(sub.join("f"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_write_crossing_the_reserved_window_checks_disk_before_writing() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct ChangingDisk(AtomicU64);
        impl DiskSpace for ChangingDisk {
            fn available(&self, _: &Path) -> Option<u64> {
                Some(self.0.load(Ordering::Relaxed))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let disk = ChangingDisk(AtomicU64::new(2 * DISK_CHECK_INTERVAL));
        let mut sink = StagingSink::create(dir.path().join("s"), u64::MAX, 100, &disk).unwrap();
        sink.write_chunk(&vec![0; DISK_CHECK_INTERVAL as usize - 1])
            .unwrap();
        disk.0.store(101, Ordering::Relaxed);
        assert!(matches!(
            sink.write_chunk(b"xx"),
            Err(EngineError::InsufficientDiskSpace { .. })
        ));
        assert_eq!(sink.written(), DISK_CHECK_INTERVAL - 1);
    }

    #[test]
    fn the_free_space_floor_is_enforced_for_small_writes() {
        let dir = tempfile::tempdir().unwrap();
        let disk = FixedDisk(Some(1_000 + 50));
        let mut sink = StagingSink::create(dir.path().join("s"), 100, 1_000, &disk).unwrap();
        assert!(matches!(
            sink.write_chunk(&[0u8; 10]),
            Err(EngineError::InsufficientDiskSpace { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_cache_directory_is_refused_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("images");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(create_private_dir(&link).is_err());
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the symlink target was not re-permissioned");
    }
}
