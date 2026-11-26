//! `land`, `tag`, `publish`, `verify`, `cleanup` (design 3.1, 3.9, 7.4,
//! 7.5, 7.13, 7.16). Landing, tagging and publishing are the only
//! irreversible steps; each first re-reads the claim, the sealed draft,
//! evidence validity and publication policy.

use super::seal::{expiry, load_seal};
use crate::release::error::{stop, Step, Stop};
use crate::release::evidence::{parse_sha256sums, tag_message, tag_seal, Receipt, ReceiptAsset};
use crate::release::forge::{Ancestry, Merge, RefWrite, ReleaseInfo};
use crate::release::plan::Plan;
use crate::release::run::Ctx;
use crate::release::util::{sha256_file, write_json};

fn main_ref(ctx: &Ctx) -> String {
    format!("heads/{}", ctx.repo.default_branch)
}

fn draft(ctx: &Ctx) -> Step<ReleaseInfo> {
    let id = ctx
        .j
        .release
        .draft_id
        .ok_or_else(|| Stop::new("state-corrupt", "no draft recorded"))?;
    ctx.w
        .forge
        .releases()?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| {
            Stop::new(
                "seal-mismatch",
                format!("sealed release {id} no longer exists"),
            )
        })
}

/// A change of publication policy on main since the attempt was pinned
/// blocks (design 7.16). Unrelated source movement does not.
///
/// The tip is fetched into the local clone first (another developer's push
/// is visible to the API before it is a local object), and the comparison
/// is bound to exactly the fetched OID. No earlier landing result exempts a
/// tip from the check: a merge commit can carry a concurrent policy change.
pub(super) fn policy(ctx: &mut Ctx) -> Step {
    let mut main = String::new();
    let mut settled = false;
    for _ in 0..3 {
        main = ctx.w.forge.get_ref(&main_ref(ctx))?.unwrap_or_default();
        if main == ctx.j.base_main
            || main == ctx.j.commit
            || ctx.j.release.policy_checked_main.as_deref() == Some(main.as_str())
        {
            return Ok(());
        }
        // A fetch failure is infrastructure, never a policy verdict.
        if ctx.w.source.fetch(&ctx.repo.default_branch)? == main {
            settled = true;
            break;
        }
    }
    if !settled {
        stop!(
            "timeout",
            "{} kept moving while its publication policy was being read",
            ctx.repo.default_branch
        );
    }
    let read_plan = |commit: &str, dir: &str| -> Step<Plan> {
        let dir = ctx.store.path(dir);
        let checkout = ctx.w.source.checkout(commit, &dir);
        let plan = checkout.and_then(|d| {
            ctx.w.tools.release_plan(&d).map_err(|e| {
                Stop::new(
                    "policy-changed",
                    format!(
                        "cannot read the publication policy at {}: {}",
                        &commit[..12.min(commit.len())],
                        e.message
                    ),
                )
            })
        });
        ctx.w.source.remove_checkout(&dir);
        Plan::parse(&plan?).map(|(p, _)| p)
    };
    let current = read_plan(&main, "policy-main")?;
    let base = read_plan(&ctx.j.base_main.clone(), "policy-base")?;
    match (current, base) {
        // Unchanged since admission, or exactly the policy R itself pins (R
        // landed, or a merge carrying R's own policy change).
        (now, then)
            if now.policy_sha256 == then.policy_sha256
                || now.policy_sha256 == ctx.j.policy_sha256 =>
        {
            // Policy at a given commit never changes: remember the verdict.
            ctx.j.release.policy_checked_main = Some(main);
            ctx.save()
        }
        (now, _) => Err(Stop::new(
            "policy-changed",
            format!(
                "main {} changed the publication policy (now {})",
                &main[..12],
                &now.policy_sha256[..12]
            ),
        )
        .with_remedy(if ctx.j.release.tag || ctx.j.release.seal_sha256.is_some() {
            "the release is sealed; its bytes cannot change - publish a new version under the new policy".to_string()
        } else {
            "run make release SUPERSEDE=1 to revalidate under the changed policy".to_string()
        })),
    }
}

/// Re-read everything an irreversible step depends on.
fn guard(ctx: &mut Ctx, step: &str) -> Step<ReleaseInfo> {
    if ctx.j.rehearsal {
        stop!(
            "rehearsal-refused",
            "a rehearsal attempt never runs `{step}`"
        );
    }
    let seal = load_seal(ctx)?;
    if ctx.j.release.confirmed_seal.as_deref() != Some(seal.digest().as_str()) {
        stop!(
            "not-confirmed",
            "seal {} was not confirmed",
            &seal.digest()[..12]
        );
    }
    let claim = ctx
        .w
        .forge
        .read_claim(&ctx.claim_key())?
        .ok_or_else(|| Stop::new("state-foreign", "the release claim disappeared"))?;
    let doc: crate::release::evidence::ClaimDoc = serde_json::from_slice(
        claim
            .files
            .get("claim.json")
            .map(Vec::as_slice)
            .unwrap_or(b"{}"),
    )
    .map_err(|e| Stop::new("state-foreign", format!("claim: {e}")))?;
    if doc.owner.id != ctx.j.owner
        || doc.seal_sha256.as_deref() != Some(seal.digest().as_str())
        || doc.commit != ctx.j.commit
    {
        stop!(
            "state-foreign",
            "the claim no longer names this owner, commit and seal"
        );
    }
    let d = draft(ctx)?;
    let mut have: Vec<_> = d
        .assets
        .iter()
        .map(|a| (a.name.clone(), a.id, a.size))
        .collect();
    let mut want: Vec<_> = seal
        .assets
        .iter()
        .map(|a| (a.name.clone(), a.asset_id, a.size))
        .collect();
    have.sort();
    want.sort();
    if have != want {
        stop!(
            "seal-mismatch",
            "release {} assets differ from the seal",
            d.id
        );
    }
    for a in &d.assets {
        let sealed = seal
            .assets
            .iter()
            .find(|s| s.asset_id == a.id)
            .map(|s| s.sha256.as_str());
        if a.digest.is_some() && a.digest.as_deref() != sealed {
            stop!(
                "seal-mismatch",
                "release asset {} digest differs from the seal",
                a.name
            );
        }
    }
    // Expiry applies to every irreversible step that is still to happen,
    // including publication after the tag exists (design 7.16). Only the
    // read-only verification of an already published release skips it,
    // and that never runs through this guard.
    expiry(ctx).map_err(|e| {
        if ctx.j.release.tag {
            e.with_remedy(format!(
                "tag {} exists but its evidence expired before publication; the draft stays unpublished — publish a new version",
                ctx.j.version
            ))
        } else {
            e
        }
    })?;
    policy(ctx)?;
    Ok(d)
}

pub fn land(ctx: &mut Ctx) -> Step {
    guard(ctx, "land")?;
    for _ in 0..3 {
        let main = ctx.w.forge.get_ref(&main_ref(ctx))?.unwrap_or_default();
        match ctx.w.forge.compare(&main, &ctx.j.commit)? {
            Ancestry::Identical | Ancestry::Behind => {
                ctx.j.release.landed_main = Some(main);
                ctx.save()?;
                return policy(ctx);
            }
            Ancestry::Ahead => {
                if let RefWrite::Done(_) =
                    ctx.w.forge.fast_forward(&main_ref(ctx), &ctx.j.commit)?
                {
                    ctx.j.release.landed_main = Some(ctx.j.commit.clone());
                    ctx.save()?;
                    return policy(ctx);
                }
            }
            Ancestry::Diverged => {
                let msg = format!("Merge release {} ({})", ctx.j.version, &ctx.j.commit[..12]);
                match ctx
                    .w
                    .forge
                    .merge(&ctx.repo.default_branch, &ctx.j.commit, &msg)?
                {
                    Merge::Merged(oid) => {
                        // Only R and its assets are release-validated.
                        ctx.store.log(
                            "land",
                            &format!("merge commit {oid} lands {}", ctx.j.commit),
                        );
                        // The merge may have absorbed a concurrent policy
                        // change on main: recheck the landed tip itself.
                        ctx.j.release.landed_main = Some(oid);
                        ctx.save()?;
                        return policy(ctx);
                    }
                    Merge::UpToDate => {}
                    Merge::Conflict => stop!(
                        "land-conflict",
                        "{} conflicts with {} on {}",
                        &ctx.j.commit[..12],
                        &main[..12],
                        ctx.repo.default_branch
                    ),
                }
            }
        }
    }
    stop!(
        "land-conflict",
        "{} kept moving while landing {}",
        ctx.repo.default_branch,
        &ctx.j.commit[..12]
    )
}

fn tag_matches(ctx: &Ctx, seal: &str) -> Step<Option<bool>> {
    Ok(ctx.w.forge.tag(&ctx.j.tag_name())?.map(|t| {
        t.annotated && t.commit == ctx.j.commit && tag_seal(&t.message).as_deref() == Some(seal)
    }))
}

pub fn tag(ctx: &mut Ctx) -> Step {
    let seal = load_seal(ctx)?.digest();
    match tag_matches(ctx, &seal)? {
        Some(true) => {
            ctx.j.release.tag = true;
            return ctx.save();
        }
        Some(false) => stop!(
            "tag-foreign",
            "tag {} exists with another commit or seal",
            ctx.j.version
        ),
        None => {}
    }
    guard(ctx, "tag")?;
    let main = ctx.w.forge.get_ref(&main_ref(ctx))?.unwrap_or_default();
    if !matches!(
        ctx.w.forge.compare(&main, &ctx.j.commit)?,
        Ancestry::Identical | Ancestry::Behind
    ) {
        stop!(
            "land-conflict",
            "{} is not on {}; landing must finish before tagging",
            &ctx.j.commit[..12],
            ctx.repo.default_branch
        );
    }
    let created = ctx.w.forge.create_tag(
        &ctx.j.tag_name(),
        &ctx.j.commit,
        &tag_message(&ctx.j.version, &seal),
    );
    match (created, tag_matches(ctx, &seal)?) {
        (_, Some(true)) => {
            ctx.j.release.tag = true;
            ctx.save()
        }
        (_, Some(false)) => stop!(
            "tag-foreign",
            "tag {} was created by someone else",
            ctx.j.version
        ),
        (Err(e), None) => Err(e),
        (Ok(_), None) => stop!(
            "network",
            "tag {} is not visible after creation",
            ctx.j.version
        ),
    }
}

pub fn publish(ctx: &mut Ctx) -> Step {
    let seal = load_seal(ctx)?.digest();
    if tag_matches(ctx, &seal)? != Some(true) {
        stop!(
            "tag-foreign",
            "tag {} does not name the release commit and seal",
            ctx.j.version
        );
    }
    if !draft(ctx)?.draft {
        ctx.j.release.published = true;
        return ctx.save();
    }
    guard(ctx, "publish")?;
    let id = ctx.j.release.draft_id.unwrap_or_default();
    if let Err(e) = ctx.w.forge.publish(id) {
        if draft(ctx)?.draft {
            return Err(Stop::new("publish-failed", e.message));
        }
    }
    if draft(ctx)?.draft {
        stop!("publish-failed", "release {id} is still a draft");
    }
    ctx.j.release.published = true;
    ctx.save()
}

/// Anonymous download of every sealed file; digests against the seal and
/// `SHA256SUMS`; attestations; tag target. Never changes release assets.
pub fn verify(ctx: &mut Ctx) -> Step {
    let seal = load_seal(ctx)?;
    let receipt_path = ctx.store.path("evidence/receipt.json");
    let release = draft(ctx)?;
    if release.draft {
        stop!("publish-failed", "release {} is not published", release.id);
    }
    if tag_matches(ctx, &seal.digest())? != Some(true) {
        stop!(
            "tag-foreign",
            "tag {} no longer names the release commit and seal",
            ctx.j.version
        );
    }
    let dir = ctx.store.path("verify");
    crate::release::util::private_dir(&dir)?;
    let mut assets = Vec::new();
    let mut downloaded = std::collections::BTreeMap::new();
    for s in &seal.assets {
        let url = release
            .assets
            .iter()
            .find(|a| a.id == s.asset_id)
            .map(|a| a.url.clone())
            .ok_or_else(|| {
                Stop::new(
                    "seal-mismatch",
                    format!("published release lacks {}", s.name),
                )
            })?;
        let dest = dir.join(&s.name);
        let mut last = None;
        for attempt in 0..3 {
            match ctx.w.forge.download_public(&url, &dest) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    ctx.w.clock.sleep(10 * (attempt + 1));
                }
            }
        }
        if let Some(e) = last {
            return Err(Stop::new("verify-unavailable", e.message));
        }
        let got = sha256_file(&dest)?;
        if got != s.sha256 {
            stop!(
                "verify-mismatch",
                "public {} is {} but the sealed bytes are {}",
                s.name,
                &got[..12],
                &s.sha256[..12]
            );
        }
        downloaded.insert(s.name.clone(), dest);
        assets.push(ReceiptAsset {
            name: s.name.clone(),
            url,
            sha256: got,
        });
    }
    let sums =
        std::fs::read_to_string(&downloaded[&ctx.plan.checksums]).map_err(anyhow::Error::from)?;
    let sums = parse_sha256sums(&sums)
        .ok_or_else(|| Stop::new("verify-mismatch", "public SHA256SUMS is malformed"))?;
    for a in &ctx.plan.assets {
        let path = &downloaded[&a.asset];
        if sums.get(&a.asset).map(String::as_str) != Some(sha256_file(path)?.as_str()) {
            stop!(
                "verify-mismatch",
                "public {} does not match SHA256SUMS",
                a.asset
            );
        }
        ctx.w
            .forge
            .verify_attestation(path, &ctx.plan.release_workflow, &ctx.j.commit)?;
    }
    let receipt = Receipt {
        schema: 1,
        outcome: "verified".into(),
        version: ctx.j.version.clone(),
        release_id: release.id,
        tag: ctx.j.tag_name(),
        commit: ctx.j.commit.clone(),
        seal_sha256: seal.digest(),
        assets,
        verified_at: ctx.w.clock.now(),
    };
    write_json(&receipt_path, &receipt)?;
    ctx.j.release.receipt_sha256 = Some(sha256_file(&receipt_path)?);
    ctx.save()
}

/// Remove disposable local copies; keep the journal, evidence, logs and
/// the receipt. A rehearsal also deletes its draft, branch and claim.
pub fn cleanup(ctx: &mut Ctx) -> Step {
    for d in ["kits", "verify"] {
        let _ = std::fs::remove_dir_all(ctx.store.path(d));
    }
    if let Ok(entries) = std::fs::read_dir(ctx.store.path("assets")) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    ctx.w.source.remove_checkout(&ctx.store.path("src"));
    if ctx.j.rehearsal {
        if let Some(id) = ctx.j.release.draft_id {
            ctx.w.forge.delete_release(id)?;
        }
        ctx.w.forge.delete_branch(&ctx.j.push_ref)?;
        ctx.w.forge.delete_claim(&ctx.claim_key())?;
    }
    Ok(())
}
