//! `make release-abandon VERSION=…`: delete this attempt's own draft, claim
//! and private state. Refused once the version tag exists; never deletes a
//! draft, branch or tag it cannot prove it owns.

use super::error::{Step, Stop};
use super::evidence::draft_marker;
use super::journal::Store;
use super::run::claim_key;
use super::world::World;
use std::path::Path;

pub fn abandon(w: &World, dir: &Path) -> Step {
    let store = Store::open(dir)?;
    let Some(j) = store.load()? else {
        return Err(Stop::new(
            "usage",
            format!("no release attempt in {}", dir.display()),
        ));
    };
    if j.release.tag
        || w.forge
            .tag(&j.version)?
            .is_some_and(|t| t.commit == j.commit)
    {
        return Err(Stop::new(
            "supersede-refused",
            format!(
                "{} is tagged; a tagged release is finished or published, never abandoned",
                j.version
            ),
        ));
    }
    super::runtime_smoke::cleanup_attempt(w, &store, &j)?;
    let marker = draft_marker(&j.operation);
    for r in w.forge.releases()? {
        if r.draft && r.body.contains(&marker) {
            w.forge.delete_release(r.id)?;
        }
    }
    let key = claim_key(&j.version, j.mode(), &j.owner);
    if let Some(c) = w.forge.read_claim(&key)? {
        let ours = c
            .files
            .get("claim.json")
            .and_then(|b| serde_json::from_slice::<super::evidence::ClaimDoc>(b).ok())
            .is_some_and(|d| d.owner.id == j.owner && d.commit == j.commit);
        if ours {
            w.forge.delete_claim(&key)?;
        }
    }
    if j.rehearsal && w.forge.get_ref(&j.push_ref)?.as_deref() == Some(j.commit.as_str()) {
        w.forge.delete_branch(&j.push_ref)?;
    }
    w.source.remove_checkout(&store.path("src"));
    drop(store);
    std::fs::remove_dir_all(dir)
        .map_err(|e| Stop::new("state-corrupt", format!("{}: {e}", dir.display())))?;
    println!(
        "abandoned {} (commit {} kept; release branch untouched)",
        j.version,
        &j.commit[..12]
    );
    Ok(())
}
