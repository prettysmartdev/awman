//! `seal` and `confirm` (design 3.7, 7.4, 7.16).
//!
//! Before the seal, only an owned draft is created or adopted and only an
//! owned incomplete upload stub is replaced. The seal freezes asset IDs and
//! digests; afterwards no file is replaced and nothing is rebuilt.

use crate::release::error::{stop, Step, Stop};
use crate::release::evidence::{draft_marker, release_evidence, sha256sums, Seal};
use crate::release::forge::{AssetInfo, ReleaseInfo};
use crate::release::journal::SealedAsset;
use crate::release::run::Ctx;
use crate::release::util::{sha256_file, write_atomic};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Name and tag of the draft: the version, or a disjoint rehearsal name.
pub fn draft_tag(ctx: &Ctx) -> String {
    if ctx.j.rehearsal {
        format!("rehearsal-{}", ctx.j.nonce)
    } else {
        ctx.j.tag_name()
    }
}

/// Evidence older than the validity window cannot authorize sealing,
/// tagging or publication; hash verification never resets its age.
pub fn expiry(ctx: &Ctx) -> Step {
    let days = ctx.plan.validity_days() as i64;
    let now = ctx.w.clock.now();
    // Every required item must carry its completion time: an unknown age
    // never authorizes anything (an empty set is not "fresh").
    let mut times: Vec<(String, Option<chrono::DateTime<chrono::Utc>>)> =
        vec![("ci.release".into(), ctx.j.ci.completed)];
    for a in &ctx.plan.assets {
        let t = ctx.j.builds.get(&a.asset).and_then(|b| b.validated);
        times.push((format!("build.{}", a.asset), t));
    }
    for n in &ctx.plan.native {
        times.push((
            n.id.clone(),
            ctx.j.native.get(&n.id).and_then(|r| r.finished),
        ));
    }
    if let Some((what, _)) = times.iter().find(|(_, t)| t.is_none()) {
        stop!(
            "evidence-expired",
            "{what} evidence has no recorded completion time, so its age is unknown"
        );
    }
    let times: Vec<(String, chrono::DateTime<chrono::Utc>)> = times
        .into_iter()
        .filter_map(|(w, t)| t.map(|t| (w, t)))
        .collect();
    if let Some((what, t)) = times.iter().min_by_key(|(_, t)| *t) {
        if now - *t > chrono::Duration::days(days) {
            stop!(
                "evidence-expired",
                "{what} evidence from {} is older than {days} days",
                t.date_naive()
            );
        }
    }
    Ok(())
}

fn files(ctx: &Ctx) -> Step<Vec<(String, PathBuf, String)>> {
    let mut out = Vec::new();
    for a in &ctx.plan.assets {
        let b = ctx
            .j
            .builds
            .get(&a.asset)
            .ok_or_else(|| Stop::new("state-corrupt", format!("{} has no build", a.asset)))?;
        let path = ctx.store.path(&format!("assets/{}", a.asset));
        let sha = b.sha256.clone().unwrap_or_default();
        if sha256_file(&path).ok().as_deref() != Some(sha.as_str()) {
            stop!(
                "retained-bytes-missing",
                "local copy of {} differs from its verified digest",
                a.asset
            );
        }
        out.push((a.asset.clone(), path, sha));
    }
    for name in [&ctx.plan.checksums, &ctx.plan.evidence] {
        let path = ctx.store.path(&format!("evidence/{name}"));
        out.push((name.clone(), path.clone(), sha256_file(&path)?));
    }
    Ok(out)
}

fn find_draft(ctx: &mut Ctx) -> Step<ReleaseInfo> {
    let tag = draft_tag(ctx);
    let marker = draft_marker(&ctx.j.operation);
    let releases = ctx.w.forge.releases()?;
    let mut ours: Vec<_> = releases
        .iter()
        .filter(|r| r.draft && r.tag == tag && r.body.contains(&marker))
        .cloned()
        .collect();
    ours.sort_by_key(|r| r.id);
    if let Some(foreign) = releases
        .iter()
        .find(|r| r.tag == tag && !ours.iter().any(|o| o.id == r.id))
    {
        stop!(
            "draft-foreign",
            "{} release {} for {tag} does not belong to this attempt; it is never deleted or reused",
            if foreign.draft { "draft" } else { "published" },
            foreign.id
        );
    }
    if let Some(d) = ours.first() {
        if ours.len() > 1 {
            ctx.store.log(
                "seal",
                &format!(
                    "adopted draft {}; extra owned drafts {:?} are left untouched",
                    d.id,
                    &ours[1..].iter().map(|r| r.id).collect::<Vec<_>>()
                ),
            );
        }
        return Ok(d.clone());
    }
    let notes = ctx
        .w
        .source
        .file_at(
            &ctx.j.commit,
            &format!("docs/releases/{}.md", ctx.j.version),
        )?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let body = format!("{notes}\n\n{marker}\n");
    match ctx.w.forge.create_draft(&tag, &tag, &ctx.j.commit, &body) {
        Ok(d) => Ok(d),
        // A lost response may still have created it: the marker finds it.
        Err(e) if e.code == "network" => {
            let again = ctx.w.forge.releases()?;
            again
                .into_iter()
                .find(|r| r.draft && r.tag == tag && r.body.contains(&marker))
                .ok_or(e)
        }
        Err(e) => Err(e),
    }
}

/// Upload missing files; keep identical ones; replace only owned stubs.
fn reconcile(
    ctx: &Ctx,
    draft: &ReleaseInfo,
    name: &str,
    path: &std::path::Path,
    sha: &str,
) -> Step<AssetInfo> {
    if let Some(a) = draft.assets.iter().find(|a| a.name == name) {
        if a.state == "uploaded" {
            if same_bytes(ctx, a, sha)? {
                return Ok(a.clone());
            }
            stop!(
                "upload-conflict",
                "draft asset {name} ({}) has bytes other than {}",
                a.id,
                &sha[..12]
            );
        }
        ctx.store.log(
            "seal",
            &format!("removing owned incomplete upload {name} ({})", a.id),
        );
        ctx.w.forge.delete_asset(a.id)?;
    }
    match ctx.w.forge.upload_asset(draft.id, name, path) {
        Ok(a) => Ok(a),
        Err(e) if e.code == "network" => {
            // Response lost after storage: reconcile on the next pass.
            Err(Stop::new(
                "network",
                format!("upload of {name}: {}", e.message),
            ))
        }
        Err(e) => Err(e),
    }
}

fn same_bytes(ctx: &Ctx, a: &AssetInfo, sha: &str) -> Step<bool> {
    if let Some(d) = &a.digest {
        return Ok(d == sha);
    }
    let tmp = ctx.store.path(&format!("evidence/.check-{}", a.id));
    ctx.w.forge.download_asset(a.id, &tmp)?;
    let same = sha256_file(&tmp)? == sha;
    let _ = std::fs::remove_file(&tmp);
    Ok(same)
}

pub fn seal(ctx: &mut Ctx) -> Step {
    super::publish::policy(ctx)?;
    if ctx.plan.native.iter().any(|n| {
        ctx.j
            .native
            .get(&n.id)
            .is_none_or(|r| r.outcome != "passed")
    }) {
        stop!(
            "native-blocked",
            "a required native check has no passing result; nothing is sealed"
        );
    }
    if ctx.j.ci.verified_sha256.is_none()
        || ctx.plan.assets.iter().any(|a| {
            ctx.j
                .builds
                .get(&a.asset)
                .is_none_or(|b| b.validated.is_none())
        })
    {
        stop!(
            "state-corrupt",
            "hosted evidence is incomplete; nothing is sealed"
        );
    }
    expiry(ctx)?;
    super::hosted::verify_hosted_records(ctx)?;
    super::hosted::verify_native_records(ctx)?;
    let sums: BTreeMap<String, String> = ctx
        .j
        .builds
        .iter()
        .map(|(n, b)| (n.clone(), b.sha256.clone().unwrap_or_default()))
        .collect();
    write_atomic(
        &ctx.store.path(&format!("evidence/{}", ctx.plan.checksums)),
        sha256sums(&sums).as_bytes(),
    )?;
    let mut ev =
        serde_json::to_vec_pretty(&release_evidence(&ctx.j)).map_err(anyhow::Error::from)?;
    ev.push(b'\n');
    write_atomic(
        &ctx.store.path(&format!("evidence/{}", ctx.plan.evidence)),
        &ev,
    )?;
    let draft = find_draft(ctx)?;
    if ctx.j.release.draft_id != Some(draft.id) {
        ctx.j.release.draft_id = Some(draft.id);
        ctx.save()?;
    }
    let expected = files(ctx)?;
    if let Some(extra) = draft
        .assets
        .iter()
        .find(|a| !expected.iter().any(|(n, _, _)| n == &a.name))
    {
        stop!(
            "upload-conflict",
            "draft {} has an unexpected asset {}; it is never deleted",
            draft.id,
            extra.name
        );
    }
    for (name, path, sha) in &expected {
        reconcile(ctx, &draft, name, path, sha)?;
    }
    // Re-read the draft from GitHub and freeze exactly what it holds.
    let draft = ctx
        .w
        .forge
        .releases()?
        .into_iter()
        .find(|r| r.id == draft.id)
        .ok_or_else(|| Stop::new("draft-foreign", format!("draft {} disappeared", draft.id)))?;
    let mut sealed = Vec::new();
    for (name, _, sha) in &expected {
        let a = draft
            .assets
            .iter()
            .find(|a| &a.name == name && a.state == "uploaded")
            .ok_or_else(|| {
                Stop::new(
                    "network",
                    format!("{name} is not yet complete on draft {}", draft.id),
                )
            })?;
        if !same_bytes(ctx, a, sha)? {
            stop!(
                "upload-conflict",
                "draft asset {name} differs from the verified bytes"
            );
        }
        sealed.push(SealedAsset {
            name: name.clone(),
            asset_id: a.id,
            sha256: sha.clone(),
            size: a.size,
        });
    }
    sealed.sort_by(|a, b| a.name.cmp(&b.name));
    let seal = Seal {
        schema: 1,
        version: ctx.j.version.clone(),
        repository_id: ctx.j.repository_id,
        commit: ctx.j.commit.clone(),
        tree: ctx.j.tree.clone(),
        plan_sha256: ctx.j.plan_sha256.clone(),
        policy_sha256: ctx.j.policy_sha256.clone(),
        release_id: draft.id,
        assets: sealed,
    };
    let digest = seal.digest();
    if let Some(prev) = &ctx.j.release.seal_sha256 {
        if prev != &digest {
            stop!(
                "seal-mismatch",
                "the draft no longer matches seal {}",
                &prev[..12]
            );
        }
    }
    write_atomic(&ctx.store.path("evidence/seal.json"), &seal.bytes())?;
    ctx.j.release.seal_sha256 = Some(digest);
    ctx.save()?;
    ctx.update_claim(Some(&seal.bytes()))
}

pub fn load_seal(ctx: &Ctx) -> Step<Seal> {
    let seal: Seal = crate::release::util::read_json(&ctx.store.path("evidence/seal.json"))
        .map_err(|e| Stop::new("state-corrupt", format!("{e:#}")))?;
    if Some(seal.digest()) != ctx.j.release.seal_sha256 {
        stop!(
            "seal-mismatch",
            "evidence/seal.json does not match the recorded seal digest"
        );
    }
    Ok(seal)
}

pub fn confirm(ctx: &mut Ctx) -> Step {
    let seal = load_seal(ctx)?;
    let digest = seal.digest();
    if ctx.j.release.confirmed_seal.as_deref() == Some(digest.as_str()) {
        return Ok(());
    }
    let main = ctx
        .w
        .forge
        .get_ref(&format!("heads/{}", ctx.repo.default_branch))?
        .unwrap_or_default();
    let landing = match ctx.w.forge.compare(&main, &ctx.j.commit)? {
        crate::release::forge::Ancestry::Ahead => "fast-forward main to the release commit",
        crate::release::forge::Ancestry::Diverged => {
            "merge commit on main (the tag stays on the release commit)"
        }
        _ => "already on main",
    };
    let mut s = format!(
        "\nRelease {} — ready to publish\n  commit   {}\n  tree     {}\n  landing  {landing}\n  seal     sha256:{digest}\n",
        ctx.j.version, ctx.j.commit, ctx.j.tree
    );
    for a in &seal.assets {
        let att = ctx
            .j
            .builds
            .get(&a.name)
            .and_then(|b| b.attestation.as_ref())
            .map(|_| "attested")
            .unwrap_or("");
        s.push_str(&format!("  {:<28} {} {att}\n", a.name, a.sha256));
    }
    s.push_str(&format!(
        "  CI       run {} ({}{} attempt {})\n",
        ctx.j.ci.dispatch.run_id.unwrap_or(0),
        if ctx.j.ci.reused { "reused" } else { "new" },
        if ctx.j.ci.auto_retried {
            ", auto-retried"
        } else {
            ""
        },
        ctx.j.ci.run_attempt
    ));
    for (id, n) in &ctx.j.native {
        s.push_str(&format!("  {id:<20} {}", n.outcome));
        if let Some(q) = &n.sbx {
            s.push_str(&format!(
                " (sbx {}, operator label {})",
                q["cli_version"].as_str().unwrap_or("?"),
                q["operator_label"].as_str().unwrap_or("?")
            ));
        }
        s.push('\n');
    }
    for line in crate::release::status::advisories(ctx.w, &ctx.plan.ci.workflow) {
        s.push_str(&format!("  {line}\n"));
    }
    let (minutes, missed) = ctx.j.target(ctx.w.clock.now());
    s.push_str(&format!(
        "  elapsed  {minutes} min{}\n",
        if missed {
            " - hour target MISSED (publication can still finish)"
        } else {
            ""
        }
    ));
    if !ctx.w.prompt.confirm(&s) {
        stop!(
            "not-confirmed",
            "publication of {} was not confirmed",
            ctx.j.version
        );
    }
    ctx.j.release.confirmed_seal = Some(digest);
    ctx.save()
}
