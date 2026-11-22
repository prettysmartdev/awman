//! Validate and decode a lease snapshot before touching disk.
use super::{ContextLimits, ContextPathError, ContextRelPath};
use base64::Engine;
use std::collections::BTreeSet;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    pub path: ContextRelPath,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StagingError {
    #[error("context file {index}: invalid path: {source}")]
    Path {
        index: usize,
        source: ContextPathError,
    },
    #[error("context file {index}: content is not valid base64")]
    Encoding { index: usize },
    #[error("context snapshot names {path} twice")]
    Duplicate { path: String },
    #[error("context snapshot uses {path} as both a file and a directory")]
    FileDirClash { path: String },
    #[error("context snapshot is {size_bytes} decoded bytes; the limit is {limit_bytes}")]
    TooLarge { size_bytes: u64, limit_bytes: u64 },
    #[error("context snapshot has {files} files; the limit is {limit}")]
    TooManyFiles { files: usize, limit: usize },
}
pub fn validate_staged_files<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a str)>,
    limits: ContextLimits,
) -> Result<Vec<StagedFile>, StagingError> {
    let mut staged = Vec::new();
    let mut names = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut total = 0u64;
    for (index, (raw, encoded)) in files.into_iter().enumerate() {
        if index >= limits.max_files {
            return Err(StagingError::TooManyFiles {
                files: index + 1,
                limit: limits.max_files,
            });
        }
        let path =
            ContextRelPath::parse(raw).map_err(|source| StagingError::Path { index, source })?;
        let folded = path.fold_key();
        if !names.insert(folded.clone()) {
            return Err(StagingError::Duplicate { path: raw.into() });
        }
        if directories.contains(&folded) {
            return Err(StagingError::FileDirClash { path: raw.into() });
        }
        for (at, _) in folded.match_indices('/') {
            let parent = &folded[..at];
            if names.contains(parent) {
                return Err(StagingError::FileDirClash {
                    path: parent.into(),
                });
            }
            directories.insert(parent.to_owned());
        }
        let cap = limits
            .max_file_bytes
            .min(limits.max_total_bytes.saturating_sub(total));
        // Padded base64 has exactly four bytes per decoded group of three.
        if encoded.len() as u64 > cap.saturating_add(2).saturating_div(3).saturating_mul(4) {
            return Err(StagingError::TooLarge {
                size_bytes: (encoded.len() as u64 / 4).saturating_mul(3),
                limit_bytes: cap,
            });
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| StagingError::Encoding { index })?;
        if bytes.len() as u64 > cap {
            return Err(StagingError::TooLarge {
                size_bytes: bytes.len() as u64,
                limit_bytes: cap,
            });
        }
        total += bytes.len() as u64;
        staged.push(StagedFile { path, bytes });
    }
    Ok(staged)
}
