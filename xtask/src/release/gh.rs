//! `GhForge`: the `Forge` trait over the operator's `gh` login and the local
//! clone (only `push` uses `git`). Every call is bounded; HTTP statuses are
//! classified once here. No token is read, printed or forwarded.

use super::error::{Step, Stop};
use super::forge::*;
use super::journal::Attestation;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct GhForge {
    pub repo: String,
    /// The clone `push` runs in.
    pub checkout: PathBuf,
}

struct ApiError {
    status: Option<u16>,
    text: String,
}

fn status_of(stderr: &str) -> Option<u16> {
    // gh prints "gh: Not Found (HTTP 404)".
    let i = stderr.rfind("(HTTP ")?;
    stderr[i + 6..].get(..3)?.parse().ok()
}

fn classify(what: &str, e: ApiError) -> Stop {
    match e.status {
        Some(401) => Stop::new(
            "gh-auth",
            format!("{what}: GitHub rejected the gh login (401)"),
        ),
        Some(403) => Stop::new(
            "gh-auth",
            format!("{what}: this gh login lacks permission (403): {}", e.text),
        ),
        None => Stop::new("network", format!("{what}: {}", e.text)),
        Some(s) => Stop::new("network", format!("{what}: HTTP {s}: {}", e.text)),
    }
}

/// Wall deadline of one API call (design 7.12); a stalled `gh` is stopped and
/// reported as `network`, so the operator can interrupt or resume.
const API_SECONDS: u64 = 120;
/// Wall deadline of one transfer (push, artifact download).
const TRANSFER_SECONDS: u64 = 900;

fn bounded(
    cmd: &mut Command,
    input: Option<&[u8]>,
    secs: u64,
) -> std::io::Result<std::process::Output> {
    super::util::output_bounded(cmd, input, std::time::Duration::from_secs(secs))
}

/// A child that could not start is a missing tool; one that ran out of time
/// is a network stall (BLOCKED, resumable).
fn spawn_error(what: &str, e: std::io::Error) -> Stop {
    if e.kind() == std::io::ErrorKind::TimedOut {
        Stop::new("network", format!("{what}: {e}"))
    } else {
        Stop::new("tool-missing", format!("{what}: {e}"))
    }
}

impl GhForge {
    fn gh(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, ApiError> {
        let mut c = Command::new("gh");
        c.args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");
        let out = bounded(&mut c, input, API_SECONDS).map_err(|e| ApiError {
            status: None,
            text: format!("gh: {e}"),
        })?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(ApiError {
            status: status_of(&text),
            text: text.lines().last().unwrap_or("").to_string(),
        })
    }

    fn api(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, ApiError> {
        let path = path.replace("{repo}", &self.repo);
        let mut args = vec!["api", "-X", method, &path];
        let bytes = body.map(|b| b.to_string().into_bytes());
        if bytes.is_some() {
            args.extend(["--input", "-"]);
        }
        let out = self.gh(&args, bytes.as_deref())?;
        if out.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&out).map_err(|e| ApiError {
            status: None,
            text: format!("unparseable GitHub response: {e}"),
        })
    }

    fn get(&self, path: &str) -> Step<Option<Value>> {
        match self.api("GET", path, None) {
            Ok(v) => Ok(Some(v)),
            Err(ApiError {
                status: Some(404), ..
            }) => Ok(None),
            Err(e) => Err(classify(path, e)),
        }
    }

    fn paginate(&self, path: &str, jq: &str) -> Step<Vec<Value>> {
        let path = path.replace("{repo}", &self.repo);
        let out = self
            .gh(&["api", "--paginate", &path, "--jq", jq], None)
            .map_err(|e| classify(&path, e))?;
        String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map_err(|e| Stop::new("network", format!("{path}: {e}")))
            })
            .collect()
    }

    fn to_file(&self, args: &[&str], dest: &Path) -> Step {
        let tmp = dest.with_extension("partial");
        let file = std::fs::File::create(&tmp)
            .map_err(|e| Stop::new("state-corrupt", format!("{}: {e}", tmp.display())))?;
        let mut cmd = Command::new("gh");
        cmd.args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .stdout(file)
            .stderr(Stdio::piped());
        let out = bounded(&mut cmd, None, TRANSFER_SECONDS).map_err(|e| spawn_error("gh", e))?;
        if !out.status.success() {
            let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(classify(
                args.last().copied().unwrap_or("download"),
                ApiError {
                    status: status_of(&text),
                    text,
                },
            ));
        }
        std::fs::rename(&tmp, dest).map_err(|e| Stop::new("state-corrupt", e.to_string()))
    }
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

fn time(v: &Value, k: &str) -> chrono::DateTime<chrono::Utc> {
    v[k].as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or_default()
}

fn run_info(v: &Value) -> RunInfo {
    let path = s(v, "path");
    RunInfo {
        id: v["id"].as_u64().unwrap_or(0),
        workflow: path
            .split('@')
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string(),
        title: s(v, "display_title"),
        head_sha: s(v, "head_sha"),
        event: s(v, "event"),
        status: s(v, "status"),
        conclusion: v["conclusion"].as_str().map(str::to_string),
        attempt: v["run_attempt"].as_u64().unwrap_or(1) as u32,
        created: time(v, "created_at"),
        updated: time(v, "updated_at"),
    }
}

fn asset_info(v: &Value) -> AssetInfo {
    AssetInfo {
        id: v["id"].as_u64().unwrap_or(0),
        name: s(v, "name"),
        size: v["size"].as_u64().unwrap_or(0),
        state: s(v, "state"),
        digest: v["digest"]
            .as_str()
            .map(|d| d.trim_start_matches("sha256:").to_string()),
        url: s(v, "browser_download_url"),
    }
}

fn release_info(v: &Value) -> ReleaseInfo {
    ReleaseInfo {
        id: v["id"].as_u64().unwrap_or(0),
        tag: s(v, "tag_name"),
        name: s(v, "name"),
        draft: v["draft"].as_bool().unwrap_or(false),
        target: s(v, "target_commitish"),
        body: s(v, "body"),
        assets: v["assets"]
            .as_array()
            .map(|a| a.iter().map(asset_info).collect())
            .unwrap_or_default(),
    }
}

/// Minimal glob for ruleset ref patterns (`*` within a segment, `**` across).
fn ref_matches(pattern: &str, name: &str) -> bool {
    if pattern == "~ALL" {
        return true;
    }
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                let deep = p.get(1) == Some(&b'*');
                let rest = if deep { &p[2..] } else { &p[1..] };
                (0..=n.len())
                    .take_while(|&i| deep || i == 0 || n[i - 1] != b'/')
                    .any(|i| go(rest, &n[i..]))
            }
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

impl Forge for GhForge {
    fn repo(&self) -> Step<RepoInfo> {
        let v = self.get("repos/{repo}")?.ok_or_else(|| {
            Stop::new(
                "wrong-repo",
                format!("{} is not visible to this gh login", self.repo),
            )
        })?;
        Ok(RepoInfo {
            id: v["id"].as_u64().unwrap_or(0),
            full_name: s(&v, "full_name"),
            default_branch: s(&v, "default_branch"),
            can_push: v["permissions"]["push"].as_bool().unwrap_or(false),
        })
    }

    fn get_ref(&self, name: &str) -> Step<Option<String>> {
        Ok(self
            .get(&format!("repos/{{repo}}/git/ref/{name}"))?
            .map(|v| s(&v["object"], "sha")))
    }

    fn compare(&self, base: &str, head: &str) -> Step<Ancestry> {
        let v = self
            .get(&format!("repos/{{repo}}/compare/{base}...{head}"))?
            .ok_or_else(|| Stop::new("network", format!("cannot compare {base}...{head}")))?;
        Ok(match v["status"].as_str() {
            Some("identical") => Ancestry::Identical,
            Some("ahead") => Ancestry::Ahead,
            Some("behind") => Ancestry::Behind,
            _ => Ancestry::Diverged,
        })
    }

    fn push(&self, commit: &str, branch_ref: &str) -> Step<RefWrite> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.checkout)
            .args(["push", "--quiet", "--no-verify", "origin"])
            .arg(format!("{commit}:refs/{branch_ref}"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out =
            bounded(&mut cmd, None, TRANSFER_SECONDS).map_err(|e| spawn_error("git push", e))?;
        if out.status.success() {
            return Ok(RefWrite::Done(commit.into()));
        }
        let text = String::from_utf8_lossy(&out.stderr);
        if text.contains("rejected") || text.contains("non-fast-forward") {
            return Ok(RefWrite::Rejected(self.get_ref(branch_ref)?));
        }
        Err(Stop::new(
            "network",
            format!("git push {branch_ref}: {}", text.trim()),
        ))
    }

    fn fast_forward(&self, branch_ref: &str, commit: &str) -> Step<RefWrite> {
        match self.api(
            "PATCH",
            &format!("repos/{{repo}}/git/refs/{branch_ref}"),
            Some(json!({"sha": commit, "force": false})),
        ) {
            Ok(_) => Ok(RefWrite::Done(commit.into())),
            Err(ApiError {
                status: Some(422 | 409),
                ..
            }) => Ok(RefWrite::Rejected(self.get_ref(branch_ref)?)),
            Err(e) => Err(classify(branch_ref, e)),
        }
    }

    fn delete_branch(&self, branch_ref: &str) -> Step {
        match self.api(
            "DELETE",
            &format!("repos/{{repo}}/git/refs/{branch_ref}"),
            None,
        ) {
            Ok(_)
            | Err(ApiError {
                status: Some(404 | 422),
                ..
            }) => Ok(()),
            Err(e) => Err(classify(branch_ref, e)),
        }
    }

    fn merge(&self, branch: &str, head: &str, message: &str) -> Step<Merge> {
        match self.api(
            "POST",
            "repos/{repo}/merges",
            Some(json!({"base": branch, "head": head, "commit_message": message})),
        ) {
            Ok(Value::Null) => Ok(Merge::UpToDate),
            Ok(v) => Ok(Merge::Merged(s(&v, "sha"))),
            Err(ApiError {
                status: Some(409), ..
            }) => Ok(Merge::Conflict),
            Err(e) => Err(classify("merge", e)),
        }
    }

    fn read_claim(&self, version: &str) -> Step<Option<Claim>> {
        let Some(oid) = self.get_ref(&format!("{CLAIM_PREFIX}{version}"))? else {
            return Ok(None);
        };
        let commit = self
            .get(&format!("repos/{{repo}}/git/commits/{oid}"))?
            .ok_or_else(|| Stop::new("network", format!("claim commit {oid} vanished")))?;
        let tree = self
            .get(&format!(
                "repos/{{repo}}/git/trees/{}",
                s(&commit["tree"], "sha")
            ))?
            .unwrap_or(Value::Null);
        let mut files = BTreeMap::new();
        for e in tree["tree"].as_array().cloned().unwrap_or_default() {
            let path = format!("repos/{}/contents/{}?ref={oid}", self.repo, s(&e, "path"));
            let bytes = self
                .gh(
                    &["api", "-H", "Accept: application/vnd.github.raw", &path],
                    None,
                )
                .map_err(|e| classify("claim", e))?;
            files.insert(s(&e, "path"), bytes);
        }
        Ok(Some(Claim { oid, files }))
    }

    fn write_claim(
        &self,
        version: &str,
        parent: Option<&str>,
        files: &BTreeMap<String, Vec<u8>>,
        message: &str,
    ) -> Step<RefWrite> {
        let mut entries = Vec::new();
        for (path, bytes) in files {
            let blob = self
                .api(
                    "POST",
                    "repos/{repo}/git/blobs",
                    Some(json!({"content": String::from_utf8_lossy(bytes), "encoding": "utf-8"})),
                )
                .map_err(|e| classify("claim blob", e))?;
            entries.push(
                json!({"path": path, "mode": "100644", "type": "blob", "sha": s(&blob, "sha")}),
            );
        }
        let tree = self
            .api(
                "POST",
                "repos/{repo}/git/trees",
                Some(json!({"tree": entries})),
            )
            .map_err(|e| classify("claim tree", e))?;
        let parents: Vec<&str> = parent.into_iter().collect();
        let commit = self
            .api(
                "POST",
                "repos/{repo}/git/commits",
                Some(json!({"message": message, "tree": s(&tree, "sha"), "parents": parents})),
            )
            .map_err(|e| classify("claim commit", e))?;
        let oid = s(&commit, "sha");
        let name = format!("{CLAIM_PREFIX}{version}");
        match parent {
            None => match self.api(
                "POST",
                "repos/{repo}/git/refs",
                Some(json!({"ref": format!("refs/{name}"), "sha": oid})),
            ) {
                Ok(_) => Ok(RefWrite::Done(oid)),
                Err(ApiError {
                    status: Some(422), ..
                }) => Ok(RefWrite::Rejected(self.get_ref(&name)?)),
                Err(e) => Err(classify("claim", e)),
            },
            Some(_) => self.fast_forward(&name, &oid).map(|w| match w {
                RefWrite::Done(_) => RefWrite::Done(oid.clone()),
                other => other,
            }),
        }
    }

    fn delete_claim(&self, version: &str) -> Step {
        self.delete_branch(&format!("{CLAIM_PREFIX}{version}"))
    }

    fn workflow_revision(&self, workflow: &str, git_ref: &str) -> Step<Option<String>> {
        Ok(self
            .get(&format!(
                "repos/{{repo}}/contents/.github/workflows/{workflow}?ref={git_ref}"
            ))?
            .map(|v| s(&v, "sha")))
    }

    fn workflow_active(&self, workflow: &str) -> Step<bool> {
        Ok(self
            .get(&format!("repos/{{repo}}/actions/workflows/{workflow}"))?
            .is_some_and(|v| v["state"] == "active"))
    }

    fn dispatch(&self, workflow: &str, git_ref: &str, inputs: &BTreeMap<String, String>) -> Step {
        self.api(
            "POST",
            &format!("repos/{{repo}}/actions/workflows/{workflow}/dispatches"),
            Some(json!({"ref": git_ref, "inputs": inputs})),
        )
        .map(|_| ())
        .map_err(|e| match e.status {
            Some(422) => Stop::new(
                "workflow-rejected",
                format!("{workflow} rejected the dispatch: {}", e.text),
            ),
            _ => classify("dispatch", e),
        })
    }

    fn runs(&self, workflow: &str, head_sha: &str) -> Step<Vec<RunInfo>> {
        Ok(self
            .paginate(
                &format!(
                    "repos/{{repo}}/actions/workflows/{workflow}/runs?head_sha={head_sha}&per_page=100"
                ),
                ".workflow_runs[]",
            )?
            .iter()
            .map(run_info)
            .collect())
    }

    fn latest_run(&self, workflow: &str, branch: &str) -> Step<Option<RunInfo>> {
        Ok(self
            .get(&format!(
                "repos/{{repo}}/actions/workflows/{workflow}/runs?branch={branch}&per_page=1"
            ))?
            .and_then(|v| v["workflow_runs"].get(0).map(run_info)))
    }

    fn run(&self, id: u64) -> Step<RunInfo> {
        self.get(&format!("repos/{{repo}}/actions/runs/{id}"))?
            .map(|v| run_info(&v))
            .ok_or_else(|| Stop::new("ci-unavailable", format!("run {id} not found")))
    }

    fn jobs(&self, run_id: u64) -> Step<Vec<JobInfo>> {
        Ok(self
            .paginate(
                &format!("repos/{{repo}}/actions/runs/{run_id}/jobs?filter=latest&per_page=100"),
                ".jobs[]",
            )?
            .iter()
            .map(|v| JobInfo {
                id: v["id"].as_u64().unwrap_or(0),
                name: s(v, "name"),
                status: s(v, "status"),
                conclusion: v["conclusion"].as_str().map(str::to_string),
                attempt: v["run_attempt"].as_u64().unwrap_or(1) as u32,
                failed_steps: v["steps"]
                    .as_array()
                    .map(|st| {
                        st.iter()
                            .filter(|x| x["conclusion"] == "failure")
                            .map(|x| s(x, "name"))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect())
    }

    fn rerun_failed(&self, run_id: u64) -> Step {
        self.api(
            "POST",
            &format!("repos/{{repo}}/actions/runs/{run_id}/rerun-failed-jobs"),
            None,
        )
        .map(|_| ())
        .map_err(|e| classify("rerun", e))
    }

    fn cancel(&self, run_id: u64) -> Step {
        match self.api(
            "POST",
            &format!("repos/{{repo}}/actions/runs/{run_id}/cancel"),
            None,
        ) {
            Ok(_)
            | Err(ApiError {
                status: Some(409), ..
            }) => Ok(()),
            Err(e) => Err(classify("cancel", e)),
        }
    }

    fn artifacts(&self, run_id: u64) -> Step<Vec<ArtifactInfo>> {
        Ok(self
            .paginate(
                &format!("repos/{{repo}}/actions/runs/{run_id}/artifacts?per_page=100"),
                ".artifacts[]",
            )?
            .iter()
            .map(|v| ArtifactInfo {
                id: v["id"].as_u64().unwrap_or(0),
                name: s(v, "name"),
                digest: v["digest"]
                    .as_str()
                    .map(|d| d.trim_start_matches("sha256:").to_string()),
                expired: v["expired"].as_bool().unwrap_or(false),
                size: v["size_in_bytes"].as_u64().unwrap_or(0),
            })
            .collect())
    }

    fn download_artifact(&self, run_id: u64, artifact: &ArtifactInfo, dest: &Path) -> Step {
        let mut cmd = Command::new("gh");
        cmd.args([
            "run",
            "download",
            &run_id.to_string(),
            "--repo",
            &self.repo,
            "-n",
            &artifact.name,
            "-D",
        ])
        .arg(dest)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let out = bounded(&mut cmd, None, TRANSFER_SECONDS)
            .map_err(|e| spawn_error("gh run download", e))?;
        if out.status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(classify(
            &artifact.name,
            ApiError {
                status: status_of(&text),
                text,
            },
        ))
    }

    fn verify_attestation(
        &self,
        file: &Path,
        signer_workflow: &str,
        source_digest: &str,
    ) -> Step<Attestation> {
        let signer = format!("{}/.github/workflows/{signer_workflow}", self.repo);
        let mut cmd = Command::new("gh");
        cmd.args(["attestation", "verify"])
            .arg(file)
            .args([
                "--repo",
                &self.repo,
                "--signer-workflow",
                &signer,
                "--source-digest",
                source_digest,
            ])
            .args(["--deny-self-hosted-runners", "--format", "json"])
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out =
            bounded(&mut cmd, None, API_SECONDS).map_err(|e| spawn_error("gh attestation", e))?;
        if !out.status.success() {
            let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if status_of(&text).is_some() || text.contains("connect") {
                return Err(classify(
                    "attestation",
                    ApiError {
                        status: status_of(&text),
                        text,
                    },
                ));
            }
            return Err(Stop::new(
                "attestation-invalid",
                format!(
                    "{}: {}",
                    file.display(),
                    text.lines().last().unwrap_or("verification failed")
                ),
            ));
        }
        Ok(Attestation {
            subject_sha256: super::util::sha256_file(file)?,
            signer_workflow: signer,
            source_digest: source_digest.into(),
            hosted: true,
        })
    }

    fn releases(&self) -> Step<Vec<ReleaseInfo>> {
        Ok(self
            .paginate("repos/{repo}/releases?per_page=100", ".[]")?
            .iter()
            .map(release_info)
            .collect())
    }

    fn create_draft(&self, tag: &str, name: &str, target: &str, body: &str) -> Step<ReleaseInfo> {
        self.api(
            "POST",
            "repos/{repo}/releases",
            Some(
                json!({"tag_name": tag, "name": name, "target_commitish": target,
                        "body": body, "draft": true, "prerelease": false}),
            ),
        )
        .map(|v| release_info(&v))
        .map_err(|e| classify("create draft", e))
    }

    fn upload_asset(&self, release_id: u64, name: &str, file: &Path) -> Step<AssetInfo> {
        let url = format!(
            "https://uploads.github.com/repos/{}/releases/{release_id}/assets?name={name}",
            self.repo
        );
        let path = file.to_string_lossy();
        let out = self
            .gh(
                &[
                    "api",
                    "-X",
                    "POST",
                    &url,
                    "-H",
                    "Content-Type: application/octet-stream",
                    "--input",
                    &path,
                ],
                None,
            )
            .map_err(|e| classify(name, e))?;
        serde_json::from_slice::<Value>(&out)
            .map(|v| asset_info(&v))
            .map_err(|e| Stop::new("network", format!("upload {name}: {e}")))
    }

    fn delete_asset(&self, asset_id: u64) -> Step {
        match self.api(
            "DELETE",
            &format!("repos/{{repo}}/releases/assets/{asset_id}"),
            None,
        ) {
            Ok(_)
            | Err(ApiError {
                status: Some(404), ..
            }) => Ok(()),
            Err(e) => Err(classify("delete asset", e)),
        }
    }

    fn download_asset(&self, asset_id: u64, dest: &Path) -> Step {
        let path = format!("repos/{}/releases/assets/{asset_id}", self.repo);
        self.to_file(
            &["api", "-H", "Accept: application/octet-stream", &path],
            dest,
        )
    }

    fn publish(&self, release_id: u64) -> Step {
        self.api(
            "PATCH",
            &format!("repos/{{repo}}/releases/{release_id}"),
            Some(json!({"draft": false, "make_latest": "true"})),
        )
        .map(|_| ())
        .map_err(|e| {
            Stop::new(
                "publish-failed",
                format!("publish release {release_id}: {}", e.text),
            )
        })
    }

    fn delete_release(&self, release_id: u64) -> Step {
        match self.api(
            "DELETE",
            &format!("repos/{{repo}}/releases/{release_id}"),
            None,
        ) {
            Ok(_)
            | Err(ApiError {
                status: Some(404), ..
            }) => Ok(()),
            Err(e) => Err(classify("delete release", e)),
        }
    }

    fn tag(&self, name: &str) -> Step<Option<TagInfo>> {
        let Some(r) = self.get(&format!("repos/{{repo}}/git/ref/tags/{name}"))? else {
            return Ok(None);
        };
        if r["object"]["type"] == "commit" {
            return Ok(Some(TagInfo {
                commit: s(&r["object"], "sha"),
                annotated: false,
                message: String::new(),
            }));
        }
        let t = self
            .get(&format!(
                "repos/{{repo}}/git/tags/{}",
                s(&r["object"], "sha")
            ))?
            .unwrap_or(Value::Null);
        Ok(Some(TagInfo {
            commit: s(&t["object"], "sha"),
            annotated: true,
            message: s(&t, "message"),
        }))
    }

    fn create_tag(&self, name: &str, commit: &str, message: &str) -> Step<RefWrite> {
        let obj = self
            .api(
                "POST",
                "repos/{repo}/git/tags",
                Some(json!({"tag": name, "message": message, "object": commit, "type": "commit"})),
            )
            .map_err(|e| classify("tag object", e))?;
        match self.api(
            "POST",
            "repos/{repo}/git/refs",
            Some(json!({"ref": format!("refs/tags/{name}"), "sha": s(&obj, "sha")})),
        ) {
            Ok(_) => Ok(RefWrite::Done(commit.into())),
            Err(ApiError {
                status: Some(422), ..
            }) => Ok(RefWrite::Rejected(None)),
            Err(e) => Err(classify("tag", e)),
        }
    }

    fn download_public(&self, url: &str, dest: &Path) -> Step {
        let out = Command::new("curl")
            .args(["-fsSL", "--max-time", "600", "--retry", "2", "-o"])
            .arg(dest)
            .arg(url)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .output()
            .map_err(|e| Stop::new("tool-missing", format!("curl: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(Stop::new(
                "verify-unavailable",
                format!(
                    "anonymous download of {url} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ))
        }
    }

    fn blocking_rules(&self, branch: &str, tag: &str) -> Step<Vec<String>> {
        let rules = self.get(&format!("repos/{{repo}}/rules/branches/{branch}"))?;
        let sets = match self.api(
            "GET",
            "repos/{repo}/rulesets?includes_parents=true&targets=tag",
            None,
        ) {
            Ok(v) => Some(v),
            Err(e) if matches!(e.status, Some(403 | 404)) => None,
            Err(e) => return Err(classify("rulesets", e)),
        };
        evaluate_rules(branch, tag, rules, sets, &mut |id| {
            self.get(&format!("repos/{{repo}}/rulesets/{id}"))
        })
    }
}

fn policy_unknown(what: String) -> Stop {
    Stop::new(
        "publication-policy-unknown",
        format!("{what}; publication authority is unknown, so nothing was started"),
    )
}

/// Decide from fetched rule data which rules would stop this login. Only a
/// successful response proves absence: a missing branch-rule response
/// (404/hidden), unreadable tag rulesets, or an active ruleset whose detail
/// cannot be read is unknown policy, never "unrestricted" (design 7.11).
fn evaluate_rules(
    branch: &str,
    tag: &str,
    branch_rules: Option<Value>,
    tag_sets: Option<Value>,
    detail: &mut dyn FnMut(&Value) -> Step<Option<Value>>,
) -> Step<Vec<String>> {
    let mut out = Vec::new();
    let Some(rules) = branch_rules else {
        return Err(policy_unknown(format!(
            "the rules for branch {branch} are not readable by this login"
        )));
    };
    let Some(sets) = tag_sets else {
        return Err(policy_unknown(format!(
            "tag rulesets are not readable by this login, so creating {tag} is unknown"
        )));
    };
    let (Some(rules), Some(sets)) = (rules.as_array(), sets.as_array()) else {
        return Err(policy_unknown(
            "GitHub returned rule data in an unexpected shape".into(),
        ));
    };
    {
        for r in rules {
            let kind = s(r, "type");
            if matches!(
                kind.as_str(),
                "pull_request"
                    | "update"
                    | "required_status_checks"
                    | "required_deployments"
                    | "required_signatures"
            ) {
                out.push(format!(
                    "branch {branch} rule `{kind}` (ruleset {})",
                    r["ruleset_id"]
                ));
            }
        }
        for set in sets {
            if set["enforcement"] != "active" {
                continue;
            }
            let Some(detail) = detail(&set["id"])? else {
                return Err(policy_unknown(format!(
                    "active tag ruleset {} is listed but its rules are not readable",
                    set["id"]
                )));
            };
            if detail["current_user_can_bypass"] == "always" {
                continue;
            }
            let full = format!("refs/tags/{tag}");
            let inc = detail["conditions"]["ref_name"]["include"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let exc = detail["conditions"]["ref_name"]["exclude"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let hit = |list: &[Value]| {
                list.iter()
                    .any(|p| p.as_str().is_some_and(|p| ref_matches(p, &full)))
            };
            if hit(&inc) && !hit(&exc) {
                for r in detail["rules"].as_array().cloned().unwrap_or_default() {
                    if matches!(s(&r, "type").as_str(), "creation" | "required_signatures") {
                        out.push(format!(
                            "tag ruleset `{}` rule `{}`",
                            s(&detail, "name"),
                            s(&r, "type")
                        ));
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_and_ruleset_patterns_are_parsed_conservatively() {
        assert_eq!(status_of("gh: Not Found (HTTP 404)"), Some(404));
        assert_eq!(status_of("dial tcp: timeout"), None);
        assert!(ref_matches("~ALL", "refs/tags/v1.0.0"));
        assert!(ref_matches("refs/tags/v*", "refs/tags/v1.0.0"));
        assert!(!ref_matches("refs/tags/x*", "refs/tags/v1.0.0"));
        assert!(ref_matches("refs/tags/**", "refs/tags/a/b"));
        assert!(!ref_matches("refs/tags/*", "refs/tags/a/b"));
    }

    #[test]
    fn only_readable_rule_data_proves_that_nothing_blocks_publication() {
        let active = json!([{"id": 7, "enforcement": "active"}]);
        let mut no_detail = |_: &Value| -> Step<Option<Value>> { Ok(None) };
        // Verified absence: both lists readable and empty.
        assert_eq!(
            evaluate_rules(
                "main",
                "v1.0.0",
                Some(json!([])),
                Some(json!([])),
                &mut no_detail
            )
            .unwrap(),
            Vec::<String>::new()
        );
        // Branch rules hidden (404) is unknown, not unrestricted.
        let e =
            evaluate_rules("main", "v1.0.0", None, Some(json!([])), &mut no_detail).unwrap_err();
        assert_eq!(e.code, "publication-policy-unknown");
        // Tag rulesets unreadable.
        let e =
            evaluate_rules("main", "v1.0.0", Some(json!([])), None, &mut no_detail).unwrap_err();
        assert_eq!(e.code, "publication-policy-unknown");
        // An active ruleset is listed but its detail is not readable.
        let e = evaluate_rules(
            "main",
            "v1.0.0",
            Some(json!([])),
            Some(active.clone()),
            &mut no_detail,
        )
        .unwrap_err();
        assert_eq!(e.code, "publication-policy-unknown");
        // A readable detail that matches the tag blocks with its name.
        let mut detail = |_: &Value| -> Step<Option<Value>> {
            Ok(Some(
                json!({"name": "tags", "conditions": {"ref_name": {"include": ["refs/tags/v*"]}},
                "rules": [{"type": "creation"}]}),
            ))
        };
        let rules = evaluate_rules(
            "main",
            "v1.0.0",
            Some(json!([{"type": "pull_request", "ruleset_id": 3}])),
            Some(active),
            &mut detail,
        )
        .unwrap();
        assert_eq!(rules.len(), 2, "{rules:?}");
    }
}
