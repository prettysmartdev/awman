//! Safe extraction of service-authenticated Actions artifact ZIPs.
//!
//! Only the subset GitHub produces is accepted: one disk, no ZIP64, no
//! encryption, stored (0) or deflate (8) entries, regular files and
//! directories. Every name must be a plain relative path. Sizes and CRC-32 are
//! checked against the central directory, and the declared total is bounded
//! before anything is written. Nothing from an archive is executed.
use anyhow::{bail, ensure, Context, Result};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::Path,
};

/// Evidence archives hold JSON, JUnit and inventories; anything larger is
/// treated as hostile (a ZIP bomb) rather than written to disk.
pub(crate) const MAX_TOTAL_BYTES: u64 = 1 << 30;
const MAX_ENTRIES: usize = 100_000;

const END_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const END_LEN: usize = 22;
const CENTRAL_LEN: usize = 46;
const LOCAL_LEN: usize = 30;

const STORED: u16 = 0;
const DEFLATE: u16 = 8;
/// General-purpose flag bits 0 (encrypted) and 6 (strong encryption).
const ENCRYPTED_FLAGS: u16 = 0x0041;

const FILE_TYPE_MASK: u32 = 0o170_000;
const REGULAR_FILE: u32 = 0o100_000;
const DIRECTORY: u32 = 0o040_000;

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let b = bytes.get(at..at + 2).context("truncated ZIP record")?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let b = bytes.get(at..at + 4).context("truncated ZIP record")?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// One central-directory record, validated before any byte is extracted.
struct Entry {
    name: String,
    directory: bool,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    local_offset: usize,
}

/// Locate the end-of-central-directory record. Its comment length must reach
/// exactly the end of the file, so a signature inside a comment cannot match.
fn end_record(zip: &[u8]) -> Result<usize> {
    ensure!(zip.len() >= END_LEN, "not a ZIP archive");
    let lowest = zip.len().saturating_sub(END_LEN + usize::from(u16::MAX));
    for at in (lowest..=zip.len() - END_LEN).rev() {
        if u32_at(zip, at)? == END_SIGNATURE
            && at + END_LEN + usize::from(u16_at(zip, at + 20)?) == zip.len()
        {
            return Ok(at);
        }
    }
    bail!("not a ZIP archive: end of central directory missing")
}

/// Reject names that could leave the destination or be interpreted
/// differently on another platform.
fn check_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "empty entry name");
    ensure!(
        !name.starts_with('/') && !name.contains('\\') && !name.contains('\0'),
        "absolute or non-portable entry name {name:?}"
    );
    let body = name.strip_suffix('/').unwrap_or(name);
    ensure!(
        body.split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "path traversal or empty component in {name:?}"
    );
    Ok(())
}

fn central_directory(zip: &[u8]) -> Result<(Vec<Entry>, usize)> {
    let end = end_record(zip)?;
    ensure!(
        end < 20 || u32_at(zip, end - 20)? != ZIP64_LOCATOR_SIGNATURE,
        "ZIP64 archives are not supported"
    );
    let (disk, central_disk) = (u16_at(zip, end + 4)?, u16_at(zip, end + 6)?);
    let (on_disk, total) = (u16_at(zip, end + 8)?, u16_at(zip, end + 10)?);
    let (size, offset) = (u32_at(zip, end + 12)?, u32_at(zip, end + 16)?);
    ensure!(
        total != u16::MAX && size != u32::MAX && offset != u32::MAX,
        "ZIP64 archives are not supported"
    );
    ensure!(
        disk == 0 && central_disk == 0 && on_disk == total,
        "multi-disk ZIP archives are not supported"
    );
    let (start, size) = (offset as usize, size as usize);
    ensure!(
        start.checked_add(size) == Some(end),
        "central directory does not end at its end record"
    );
    let count = usize::from(total);
    ensure!(count <= MAX_ENTRIES, "too many ZIP entries");
    let mut entries = Vec::with_capacity(count);
    let mut names = BTreeSet::new();
    let mut total_size = 0u64;
    let mut at = start;
    for _ in 0..count {
        ensure!(
            u32_at(zip, at)? == CENTRAL_SIGNATURE,
            "corrupt central directory"
        );
        let flags = u16_at(zip, at + 8)?;
        let method = u16_at(zip, at + 10)?;
        let crc = u32_at(zip, at + 16)?;
        let compressed = u32_at(zip, at + 20)?;
        let uncompressed = u32_at(zip, at + 24)?;
        let name_len = usize::from(u16_at(zip, at + 28)?);
        let extra_len = usize::from(u16_at(zip, at + 30)?);
        let comment_len = usize::from(u16_at(zip, at + 32)?);
        let entry_disk = u16_at(zip, at + 34)?;
        let external = u32_at(zip, at + 38)?;
        let local_offset = u32_at(zip, at + 42)?;
        let name_bytes = zip
            .get(at + CENTRAL_LEN..at + CENTRAL_LEN + name_len)
            .context("truncated entry name")?;
        let name = std::str::from_utf8(name_bytes)
            .context("entry name is not UTF-8")?
            .to_string();
        at += CENTRAL_LEN + name_len + extra_len + comment_len;
        ensure!(at <= end, "central directory overruns its end record");

        check_name(&name)?;
        ensure!(flags & ENCRYPTED_FLAGS == 0, "encrypted entry {name:?}");
        ensure!(
            compressed != u32::MAX && uncompressed != u32::MAX && local_offset != u32::MAX,
            "ZIP64 entry {name:?} is not supported"
        );
        ensure!(entry_disk == 0, "multi-disk entry {name:?}");
        ensure!(
            method == STORED || method == DEFLATE,
            "entry {name:?} uses unsupported compression method {method}"
        );
        // Unix mode bits live in the high half; 0 means "not recorded".
        let file_type = (external >> 16) & FILE_TYPE_MASK;
        let directory = name.ends_with('/');
        ensure!(
            file_type == 0 || file_type == if directory { DIRECTORY } else { REGULAR_FILE },
            "entry {name:?} is a link or other non-regular file"
        );
        ensure!(
            !directory || uncompressed == 0,
            "directory entry {name:?} has content"
        );
        ensure!(
            names.insert(name.trim_end_matches('/').to_string()),
            "duplicate entry {name:?}"
        );
        total_size += u64::from(uncompressed);
        ensure!(
            total_size <= MAX_TOTAL_BYTES,
            "archive expands beyond {MAX_TOTAL_BYTES} bytes"
        );
        entries.push(Entry {
            name,
            directory,
            method,
            crc,
            compressed: u64::from(compressed),
            size: u64::from(uncompressed),
            local_offset: local_offset as usize,
        });
    }
    ensure!(at == end, "central directory has trailing bytes");
    Ok((entries, start))
}

/// Return the compressed bytes for one entry, cross-checked against its local header.
fn entry_data<'a>(zip: &'a [u8], entry: &Entry, central_start: usize) -> Result<&'a [u8]> {
    let at = entry.local_offset;
    ensure!(
        u32_at(zip, at)? == LOCAL_SIGNATURE,
        "missing local header for {:?}",
        entry.name
    );
    ensure!(
        u16_at(zip, at + 6)? & ENCRYPTED_FLAGS == 0 && u16_at(zip, at + 8)? == entry.method,
        "local header for {:?} disagrees with the central directory",
        entry.name
    );
    let name_len = usize::from(u16_at(zip, at + 26)?);
    let extra_len = usize::from(u16_at(zip, at + 28)?);
    ensure!(
        zip.get(at + LOCAL_LEN..at + LOCAL_LEN + name_len) == Some(entry.name.as_bytes()),
        "local name for {:?} disagrees with the central directory",
        entry.name
    );
    let start = at + LOCAL_LEN + name_len + extra_len;
    let end = start
        .checked_add(entry.compressed as usize)
        .context("entry size overflow")?;
    ensure!(
        end <= central_start,
        "entry {:?} overlaps the central directory",
        entry.name
    );
    Ok(&zip[start..end])
}

fn inflate(entry: &Entry, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match entry.method {
        STORED => {
            ensure!(
                entry.compressed == entry.size,
                "stored entry {:?} has inconsistent sizes",
                entry.name
            );
            out.extend_from_slice(data);
        }
        _ => {
            // Read at most one byte more than declared, so an understated size
            // is detected without inflating an unbounded stream.
            flate2::read::DeflateDecoder::new(data)
                .take(entry.size + 1)
                .read_to_end(&mut out)
                .with_context(|| format!("corrupt deflate data in {:?}", entry.name))?;
        }
    }
    ensure!(
        out.len() as u64 == entry.size,
        "entry {:?} size differs from the central directory",
        entry.name
    );
    let mut crc = flate2::Crc::new();
    crc.update(&out);
    ensure!(
        crc.sum() == entry.crc,
        "CRC-32 mismatch for {:?}",
        entry.name
    );
    Ok(out)
}

/// Validate the complete archive, then extract it below `dest`. Files are
/// created exclusively, so nothing pre-existing is followed or overwritten.
pub(crate) fn extract(zip: &[u8], dest: &Path) -> Result<()> {
    let (entries, central_start) = central_directory(zip)?;
    // Decompress and verify everything before writing, so a rejected archive
    // leaves no partial evidence behind.
    let mut contents = Vec::with_capacity(entries.len());
    for entry in &entries {
        let data = entry_data(zip, entry, central_start)?;
        contents.push(if entry.directory {
            None
        } else {
            Some(inflate(entry, data)?)
        });
    }
    for (entry, content) in entries.iter().zip(contents) {
        let path = dest.join(entry.name.trim_end_matches('/'));
        match content {
            None => fs::create_dir_all(&path)?,
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .with_context(|| format!("cannot create {:?}", entry.name))?
                    .write_all(&bytes)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One archive member for the in-test writer.
    pub(crate) struct Member<'a> {
        pub name: &'a str,
        pub data: &'a [u8],
        pub deflate: bool,
        /// Unix mode in the external attributes' high half.
        pub mode: u32,
        pub flags: u16,
    }
    impl<'a> Member<'a> {
        pub fn file(name: &'a str, data: &'a [u8]) -> Self {
            Self {
                name,
                data,
                deflate: false,
                mode: 0o100_644,
                flags: 0,
            }
        }
        pub fn deflated(name: &'a str, data: &'a [u8]) -> Self {
            Self {
                deflate: true,
                ..Self::file(name, data)
            }
        }
    }

    /// Minimal ZIP writer used to build hostile and valid archives in tests.
    pub(crate) fn write_zip(members: &[Member]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for m in members {
            let mut crc = flate2::Crc::new();
            crc.update(m.data);
            let body = if m.deflate {
                let mut e =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(m.data).unwrap();
                e.finish().unwrap()
            } else {
                m.data.to_vec()
            };
            let method: u16 = if m.deflate { DEFLATE } else { STORED };
            let offset = out.len() as u32;
            let common = |buf: &mut Vec<u8>| {
                buf.extend(m.flags.to_le_bytes());
                buf.extend(method.to_le_bytes());
                buf.extend([0u8; 4]); // time, date
                buf.extend(crc.sum().to_le_bytes());
                buf.extend((body.len() as u32).to_le_bytes());
                buf.extend((m.data.len() as u32).to_le_bytes());
                buf.extend((m.name.len() as u16).to_le_bytes());
                buf.extend(0u16.to_le_bytes()); // extra
            };
            out.extend(LOCAL_SIGNATURE.to_le_bytes());
            out.extend(20u16.to_le_bytes());
            common(&mut out);
            out.extend(m.name.as_bytes());
            out.extend(&body);
            central.extend(CENTRAL_SIGNATURE.to_le_bytes());
            central.extend((0x0300u16 | 20).to_le_bytes()); // made by Unix
            central.extend(20u16.to_le_bytes());
            common(&mut central);
            central.extend([0u8; 6]); // comment, disk, internal attributes
            central.extend((m.mode << 16).to_le_bytes());
            central.extend(offset.to_le_bytes());
            central.extend(m.name.as_bytes());
        }
        let start = out.len() as u32;
        out.extend(&central);
        out.extend(END_SIGNATURE.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend((members.len() as u16).to_le_bytes());
        out.extend((members.len() as u16).to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    fn extract_new(zip: &[u8]) -> (tempfile::TempDir, Result<()>) {
        let dir = tempfile::tempdir().unwrap();
        let result = extract(zip, &dir.path().join("out"));
        (dir, result)
    }

    #[test]
    fn stored_and_deflated_entries_extract_with_verified_contents() {
        let big = "junit ".repeat(10_000);
        let mut directory = Member::file("nested/", b"");
        directory.mode = 0o040_755;
        let mut unrecorded_mode = Member::file("plain.txt", b"plain");
        unrecorded_mode.mode = 0;
        let zip = write_zip(&[
            Member::file("run.json", b"{\"ok\":true}"),
            directory,
            Member::deflated("nested/junit.xml", big.as_bytes()),
            unrecorded_mode,
        ]);
        let (dir, result) = extract_new(&zip);
        result.unwrap();
        let out = dir.path().join("out");
        assert_eq!(fs::read(out.join("run.json")).unwrap(), b"{\"ok\":true}");
        assert_eq!(
            fs::read_to_string(out.join("nested/junit.xml")).unwrap(),
            big
        );
        assert_eq!(fs::read(out.join("plain.txt")).unwrap(), b"plain");
        // An empty archive is valid; required files are checked by callers.
        assert!(extract_new(&write_zip(&[])).1.is_ok());
    }

    #[test]
    fn unsafe_names_links_duplicates_and_encryption_are_rejected_before_writing() {
        let mut link = Member::file("link", b"/etc/passwd");
        link.mode = 0o120_777;
        let mut fifo = Member::file("fifo", b"");
        fifo.mode = 0o010_644;
        let mut encrypted = Member::file("secret", b"x");
        encrypted.flags = 1;
        let mut file_marked_dir = Member::file("dir-but-file", b"x");
        file_marked_dir.mode = 0o040_755;
        let mut same_name_dir = Member::file("x/", b"");
        same_name_dir.mode = 0o040_755;
        let cases: Vec<(&str, Vec<Member>)> = vec![
            ("absolute", vec![Member::file("/etc/x", b"x")]),
            ("parent", vec![Member::file("../x", b"x")]),
            ("nested parent", vec![Member::file("a/../../x", b"x")]),
            ("dot", vec![Member::file("./x", b"x")]),
            ("empty component", vec![Member::file("a//x", b"x")]),
            ("backslash", vec![Member::file("a\\..\\x", b"x")]),
            ("empty", vec![Member::file("", b"x")]),
            (
                "duplicate",
                vec![Member::file("x", b"1"), Member::deflated("x", b"2")],
            ),
            (
                "file and directory with one name",
                vec![Member::file("x", b"1"), same_name_dir],
            ),
            ("symlink", vec![link]),
            ("fifo", vec![fifo]),
            ("file with directory mode", vec![file_marked_dir]),
            ("encrypted", vec![encrypted]),
        ];
        for (case, members) in cases {
            let mut members = members;
            // A valid sibling first proves nothing is written on rejection.
            members.insert(0, Member::file("valid.json", b"{}"));
            let (dir, result) = extract_new(&write_zip(&members));
            assert!(result.is_err(), "accepted {case}");
            assert!(
                !dir.path().join("out").exists(),
                "{case}: wrote before rejecting"
            );
        }
    }

    #[test]
    fn corrupt_sizes_crc_methods_and_zip64_are_rejected() {
        let good = write_zip(&[Member::deflated("a.json", b"{\"tests\":1}")]);
        let start = u32_at(&good, good.len() - 6).unwrap() as usize;
        let patch = |at: usize, bytes: &[u8]| {
            let mut zip = good.clone();
            zip[at..at + bytes.len()].copy_from_slice(bytes);
            zip
        };
        let cases = [
            ("crc", patch(start + 16, &0u32.to_le_bytes())),
            ("understated size", patch(start + 24, &3u32.to_le_bytes())),
            ("overstated size", patch(start + 24, &500u32.to_le_bytes())),
            (
                "unsupported method",
                patch(start + 10, &12u16.to_le_bytes()),
            ),
            ("method disagrees", patch(8, &0u16.to_le_bytes())),
            ("zip64 size", patch(start + 24, &u32::MAX.to_le_bytes())),
            (
                "zip64 end record",
                patch(good.len() - 6, &u32::MAX.to_le_bytes()),
            ),
            ("multi-disk", patch(good.len() - 18, &1u16.to_le_bytes())),
            ("truncated", good[..good.len() - 1].to_vec()),
            ("not a zip", b"definitely not a zip archive".to_vec()),
        ];
        for (case, zip) in cases {
            assert!(extract_new(&zip).1.is_err(), "accepted {case}");
        }
        assert!(extract_new(&good).1.is_ok());
    }

    #[test]
    fn declared_expansion_beyond_the_bound_is_rejected_without_inflating() {
        let zip = write_zip(&[
            Member::deflated("a", b"aaaa"),
            Member::deflated("b", b"bbbb"),
        ]);
        let start = u32_at(&zip, zip.len() - 6).unwrap() as usize;
        let mut bomb = zip.clone();
        // Each declares half the bound plus one byte: together they exceed it.
        let half = (MAX_TOTAL_BYTES / 2 + 1) as u32;
        bomb[start + 24..start + 28].copy_from_slice(&half.to_le_bytes());
        let second = start + CENTRAL_LEN + 1;
        bomb[second + 24..second + 28].copy_from_slice(&half.to_le_bytes());
        let error = extract_new(&bomb).1.unwrap_err().to_string();
        assert!(error.contains("expands beyond"), "{error}");
    }
}
