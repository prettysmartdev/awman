//! Store operations shared by the pipeline subcommands: finding a candidate's
//! draft, compare-and-check state writes and asset replacement.

use super::args::Stop;
use super::decide::Existing;
use super::hub::{Hub, ReleaseFact};
use super::record::{draft_tag, ReleaseRecord, STATE_ASSET};
use anyhow::{Context, Result};
use std::path::Path;

/// The release holding candidate `candidate` of `version` in `namespace`.
pub fn find(
    hub: &dyn Hub,
    version: &str,
    namespace: &str,
    candidate: u32,
) -> Result<Result<Existing, Stop>> {
    let tag = draft_tag(version, namespace);
    let all = super::existing(hub, &tag)?;
    Ok(all
        .into_iter()
        .find(|e| e.candidate() == Some(candidate))
        .ok_or_else(|| {
            Stop::Failed(format!(
                "no release for candidate {candidate} of {version} ({namespace}); it was \
                 abandoned or superseded. Start a new candidate"
            ))
        }))
}

/// Upload `path` as `name`, replacing an existing asset of that name. Only
/// for journal files (state, evidence); never for a shipped binary.
pub fn replace_asset(hub: &dyn Hub, release: &ReleaseFact, name: &str, path: &Path) -> Result<()> {
    anyhow::ensure!(
        release.draft,
        "assets of a published release are never changed"
    );
    if let Some(a) = release.asset(name) {
        hub.delete_asset(a.id)?;
    }
    hub.upload_asset(release.id, name, path)?;
    Ok(())
}

/// Write `rec` as the draft's `release-state.json` after checking that the
/// state on the store still belongs to the same candidate identity
/// (compare-and-check; callers hold the publication lock).
pub fn put_state(
    hub: &dyn Hub,
    release_id: u64,
    rec: &ReleaseRecord,
    out: &Path,
) -> Result<Result<(), Stop>> {
    let release = hub
        .releases()?
        .into_iter()
        .find(|r| r.id == release_id)
        .context("the candidate's draft disappeared")?;
    if let Some(current) = super::read_record(hub, &release)? {
        if current.identity() != rec.identity() || current.candidate != rec.candidate {
            return Ok(Err(Stop::Blocked(format!(
                "release-state.json of draft {release_id} changed underneath this run (now \
                 candidate {} at {}); nothing was published. Rerun to re-read the state",
                current.candidate, current.commit
            ))));
        }
    }
    let path = out.join(STATE_ASSET);
    super::args::write_json(&path, rec)?;
    replace_asset(hub, &release, STATE_ASSET, &path)?;
    Ok(Ok(()))
}

/// Download one asset and return its SHA-256.
pub fn download(
    hub: &dyn Hub,
    release: &ReleaseFact,
    name: &str,
    dest: &Path,
) -> Result<Option<String>> {
    let Some(a) = release.asset(name) else {
        return Ok(None);
    };
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    hub.download_asset(a.id, dest)?;
    Ok(Some(crate::evidence::digest::sha256_file(dest)?))
}

/// Make a file read-only (immutable candidate storage).
pub fn read_only(path: &Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}
