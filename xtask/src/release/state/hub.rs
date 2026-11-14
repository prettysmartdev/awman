//! Where release state lives. [`GhHub`] is GitHub through the `gh` CLI; it is
//! the only store the workflows and `make release` use. [`DirHub`] is a
//! directory with the same semantics for the pipeline tests and local
//! rehearsal (`--store DIR`); it can never publish anything and its records
//! carry repository id `local`, which no real promotion accepts.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetFact {
    pub id: u64,
    pub name: String,
    pub size: u64,
    /// `sha256:<hex>` as reported by the service, when it reports one.
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseFact {
    pub id: u64,
    pub tag: String,
    pub name: String,
    pub draft: bool,
    pub target_commitish: String,
    pub body: String,
    pub url: String,
    pub assets: Vec<AssetFact>,
}

impl ReleaseFact {
    pub fn asset(&self, name: &str) -> Option<&AssetFact> {
        self.assets.iter().find(|a| a.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFact {
    pub id: u64,
    pub title: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub created_at: String,
    pub head_branch: String,
    pub event: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFact {
    pub id: u64,
    pub name: String,
    pub run_id: u64,
    pub head_branch: String,
    pub expired: bool,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ReleaseEdit {
    pub draft: Option<bool>,
    pub name: Option<String>,
    pub body: Option<String>,
}

pub trait Hub {
    fn repo(&self) -> &str;
    fn repository_id(&self) -> Result<String>;
    /// Every release, drafts included.
    fn releases(&self) -> Result<Vec<ReleaseFact>>;
    fn create_draft(&self, tag: &str, name: &str, commit: &str, body: &str) -> Result<ReleaseFact>;
    fn edit_release(&self, id: u64, edit: &ReleaseEdit) -> Result<ReleaseFact>;
    fn delete_release(&self, id: u64) -> Result<()>;
    fn upload_asset(&self, release: u64, name: &str, path: &Path) -> Result<AssetFact>;
    fn delete_asset(&self, asset: u64) -> Result<()>;
    fn download_asset(&self, asset: u64, dest: &Path) -> Result<()>;
    /// Commit a tag points at (peeled), if the tag exists on origin.
    fn tag(&self, name: &str) -> Result<Option<String>>;
    /// Annotated tag at `commit`, through the Git data API.
    fn create_tag(&self, name: &str, commit: &str, message: &str) -> Result<()>;
    fn main_head(&self) -> Result<String>;
    /// Materialize policy at a freshly resolved service main tip, never trust a stale checkout.
    fn policy_checkout(
        &self,
        checkout: &Path,
        _out: &Path,
        _version: &str,
    ) -> Result<(String, PathBuf)> {
        Ok((self.main_head()?, checkout.to_owned()))
    }
    fn is_ancestor_of_main(&self, commit: &str) -> Result<bool>;
    fn runs(&self, workflow: &str) -> Result<Vec<RunFact>>;
    /// Unexpired artifacts whose name starts with `prefix`.
    fn artifacts(&self, prefix: &str) -> Result<Vec<ArtifactFact>>;
    fn download_artifact(&self, artifact: &ArtifactFact, dir: &Path) -> Result<()>;
    /// Download from the public release URL, without credentials.
    fn public_download(&self, tag: &str, name: &str, dest: &Path) -> Result<()>;
    /// Whether `path` carries a build provenance attestation produced by this
    /// repository's candidate workflow on protected main.
    fn verify_attestation(&self, path: &Path) -> Result<bool>;
    /// Independently authenticate the exact evidence artifact and producer job.
    fn authenticate_cell(
        &self,
        evidence: &crate::evidence::schema::Evidence,
        file: &Path,
    ) -> Result<bool>;
}

/// Open the store: `--store DIR` selects a [`DirHub`], otherwise GitHub.
pub fn open(repo: &str, store: Option<&str>) -> Result<Box<dyn Hub>> {
    match store {
        Some(dir) => Ok(Box::new(DirHub::open(Path::new(dir), repo)?)),
        None => Ok(Box::new(GhHub::new(repo))),
    }
}

/// `owner/name` of the repository: `--repo`, else `$GITHUB_REPOSITORY`.
pub fn repo_from(flag: Option<&str>) -> Result<String> {
    let repo = flag
        .map(str::to_owned)
        .or_else(|| std::env::var("GITHUB_REPOSITORY").ok())
        .context("--repo owner/name (or GITHUB_REPOSITORY) is required")?;
    ensure!(
        repo.split('/').count() == 2 && !repo.contains(".."),
        "repository must be owner/name, got {repo}"
    );
    Ok(repo)
}

// ── GitHub ───────────────────────────────────────────────────────────────────

pub struct GhHub {
    repo: String,
}

/// A `gh api` call that the service answered with 404.
#[derive(Debug)]
pub struct NotFound;
impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not found (HTTP 404)")
    }
}
impl std::error::Error for NotFound {}

impl GhHub {
    pub fn new(repo: &str) -> GhHub {
        GhHub { repo: repo.into() }
    }

    fn api(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut cmd = Command::new("gh");
        cmd.args([
            "api",
            "-X",
            method,
            "-H",
            "Accept: application/vnd.github+json",
        ]);
        cmd.arg(path);
        if body.is_some() {
            cmd.args(["--input", "-"]);
        }
        let out = run_with_stdin(cmd, body.map(|b| b.to_string().into_bytes()))?;
        parse_api(path, out)
    }

    fn pages(&self, path: &str) -> Result<Vec<Value>> {
        let mut cmd = Command::new("gh");
        cmd.args(["api", "--paginate", "--slurp", path]);
        let pages = parse_api(path, run_with_stdin(cmd, None)?)?;
        Ok(pages
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .flat_map(|p| match p {
                Value::Array(items) => items,
                Value::Object(o) => o
                    .into_iter()
                    .filter_map(|(_, v)| v.as_array().cloned())
                    .flatten()
                    .collect(),
                _ => Vec::new(),
            })
            .collect())
    }
}

fn run_with_stdin(mut cmd: Command, stdin: Option<Vec<u8>>) -> Result<std::process::Output> {
    use std::io::Write;
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("running gh (is the GitHub CLI installed?)")?;
    if let Some(bytes) = stdin {
        child.stdin.take().expect("piped").write_all(&bytes)?;
    }
    Ok(child.wait_with_output()?)
}

/// Turn a `gh api` result into JSON, mapping 404 to [`NotFound`].
pub fn parse_api(path: &str, out: std::process::Output) -> Result<Value> {
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("HTTP 404") {
            return Err(NotFound.into());
        }
        bail!("gh api {path} failed: {}", stderr.trim());
    }
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&out.stdout).with_context(|| format!("gh api {path}: invalid JSON"))
}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NotFound>().is_some()
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}
fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// Parse one release object of the REST API.
pub fn release_of(v: &Value) -> ReleaseFact {
    ReleaseFact {
        id: u(v, "id"),
        tag: s(v, "tag_name"),
        name: s(v, "name"),
        draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false),
        target_commitish: s(v, "target_commitish"),
        body: s(v, "body"),
        url: s(v, "html_url"),
        assets: v
            .get("assets")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(asset_of).collect())
            .unwrap_or_default(),
    }
}

pub fn asset_of(v: &Value) -> AssetFact {
    AssetFact {
        id: u(v, "id"),
        name: s(v, "name"),
        size: u(v, "size"),
        digest: v.get("digest").and_then(Value::as_str).map(str::to_owned),
    }
}

pub fn run_of(v: &Value) -> RunFact {
    RunFact {
        id: u(v, "id"),
        title: s(v, "display_title"),
        status: s(v, "status"),
        conclusion: v
            .get("conclusion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        created_at: s(v, "created_at"),
        head_branch: s(v, "head_branch"),
        event: s(v, "event"),
        url: s(v, "html_url"),
    }
}

pub fn artifact_of(v: &Value) -> ArtifactFact {
    let run = v.get("workflow_run").cloned().unwrap_or(Value::Null);
    ArtifactFact {
        id: u(v, "id"),
        name: s(v, "name"),
        run_id: u(&run, "id"),
        head_branch: s(&run, "head_branch"),
        expired: v.get("expired").and_then(Value::as_bool).unwrap_or(false),
        digest: v.get("digest").and_then(Value::as_str).map(str::to_owned),
    }
}

impl Hub for GhHub {
    fn repo(&self) -> &str {
        &self.repo
    }
    fn repository_id(&self) -> Result<String> {
        let v = self.api("GET", &format!("repos/{}", self.repo), None)?;
        let id = u(&v, "id");
        ensure!(id != 0, "repository id unavailable");
        Ok(id.to_string())
    }
    fn releases(&self) -> Result<Vec<ReleaseFact>> {
        Ok(self
            .pages(&format!("repos/{}/releases?per_page=100", self.repo))?
            .iter()
            .map(release_of)
            .collect())
    }
    fn create_draft(&self, tag: &str, name: &str, commit: &str, body: &str) -> Result<ReleaseFact> {
        let v = self.api(
            "POST",
            &format!("repos/{}/releases", self.repo),
            Some(
                &json!({"tag_name": tag, "name": name, "target_commitish": commit,
                         "body": body, "draft": true, "prerelease": false}),
            ),
        )?;
        Ok(release_of(&v))
    }
    fn edit_release(&self, id: u64, edit: &ReleaseEdit) -> Result<ReleaseFact> {
        let mut body = serde_json::Map::new();
        if let Some(d) = edit.draft {
            body.insert("draft".into(), json!(d));
        }
        if let Some(n) = &edit.name {
            body.insert("name".into(), json!(n));
        }
        if let Some(b) = &edit.body {
            body.insert("body".into(), json!(b));
        }
        let v = self.api(
            "PATCH",
            &format!("repos/{}/releases/{id}", self.repo),
            Some(&Value::Object(body)),
        )?;
        Ok(release_of(&v))
    }
    fn delete_release(&self, id: u64) -> Result<()> {
        self.api(
            "DELETE",
            &format!("repos/{}/releases/{id}", self.repo),
            None,
        )?;
        Ok(())
    }
    fn upload_asset(&self, release: u64, name: &str, path: &Path) -> Result<AssetFact> {
        let mut cmd = Command::new("gh");
        cmd.args([
            "api",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/octet-stream",
        ])
        .arg(format!(
            "https://uploads.github.com/repos/{}/releases/{release}/assets?name={name}",
            self.repo
        ))
        .arg("--input")
        .arg(path);
        Ok(asset_of(&parse_api("upload", run_with_stdin(cmd, None)?)?))
    }
    fn delete_asset(&self, asset: u64) -> Result<()> {
        self.api(
            "DELETE",
            &format!("repos/{}/releases/assets/{asset}", self.repo),
            None,
        )?;
        Ok(())
    }
    fn download_asset(&self, asset: u64, dest: &Path) -> Result<()> {
        let file = std::fs::File::create(dest)?;
        let status = Command::new("gh")
            .args(["api", "-H", "Accept: application/octet-stream"])
            .arg(format!("repos/{}/releases/assets/{asset}", self.repo))
            .stdout(file)
            .status()?;
        ensure!(status.success(), "downloading release asset {asset} failed");
        Ok(())
    }
    fn tag(&self, name: &str) -> Result<Option<String>> {
        let v = match self.api(
            "GET",
            &format!("repos/{}/git/ref/tags/{name}", self.repo),
            None,
        ) {
            Ok(v) => v,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let obj = v.get("object").cloned().unwrap_or(Value::Null);
        if s(&obj, "type") == "tag" {
            let t = self.api(
                "GET",
                &format!("repos/{}/git/tags/{}", self.repo, s(&obj, "sha")),
                None,
            )?;
            return Ok(Some(s(t.get("object").unwrap_or(&Value::Null), "sha")));
        }
        Ok(Some(s(&obj, "sha")))
    }
    fn create_tag(&self, name: &str, commit: &str, message: &str) -> Result<()> {
        let t = self.api(
            "POST",
            &format!("repos/{}/git/tags", self.repo),
            Some(&json!({"tag": name, "message": message, "object": commit, "type": "commit"})),
        )?;
        let sha = s(&t, "sha");
        ensure!(!sha.is_empty(), "tag object was not created");
        self.api(
            "POST",
            &format!("repos/{}/git/refs", self.repo),
            Some(&json!({"ref": format!("refs/tags/{name}"), "sha": sha})),
        )?;
        Ok(())
    }
    fn policy_checkout(
        &self,
        _checkout: &Path,
        out: &Path,
        version: &str,
    ) -> Result<(String, PathBuf)> {
        let tip = self.main_head()?;
        let dest = out.join("current-main-policy");
        for path in [
            "ci/gates.toml".to_owned(),
            format!("docs/releases/{version}.md"),
            "packaging/macos/awman.entitlements".to_owned(),
        ] {
            // gh supplies authentication without placing a token in argv or
            // persisting it in a checkout, and also works for private repositories.
            let output = Command::new("gh")
                .args(["api", "-H", "Accept: application/vnd.github.raw+json"])
                .arg(format!("repos/{}/contents/{path}?ref={tip}", self.repo))
                .output()?;
            ensure!(
                output.status.success(),
                "cannot read current main release policy {path}"
            );
            let file = dest.join(path);
            std::fs::create_dir_all(file.parent().context("policy path parent")?)?;
            std::fs::write(file, output.stdout)?;
        }
        ensure!(
            self.main_head()? == tip,
            "protected main moved while reading policy; retry verification"
        );
        Ok((tip, dest))
    }
    fn main_head(&self) -> Result<String> {
        let v = self.api(
            "GET",
            &format!("repos/{}/git/ref/heads/main", self.repo),
            None,
        )?;
        let sha = s(v.get("object").unwrap_or(&Value::Null), "sha");
        ensure!(sha.len() == 40, "could not read main");
        Ok(sha)
    }
    fn is_ancestor_of_main(&self, commit: &str) -> Result<bool> {
        let v = match self.api(
            "GET",
            &format!("repos/{}/compare/{commit}...main", self.repo),
            None,
        ) {
            Ok(v) => v,
            Err(e) if is_not_found(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        Ok(matches!(s(&v, "status").as_str(), "ahead" | "identical"))
    }
    fn runs(&self, workflow: &str) -> Result<Vec<RunFact>> {
        let v = self.api(
            "GET",
            &format!(
                "repos/{}/actions/workflows/{workflow}/runs?per_page=100",
                self.repo
            ),
            None,
        )?;
        Ok(v.get("workflow_runs")
            .and_then(Value::as_array)
            .map(|r| r.iter().map(run_of).collect())
            .unwrap_or_default())
    }
    fn artifacts(&self, prefix: &str) -> Result<Vec<ArtifactFact>> {
        Ok(self
            .pages(&format!(
                "repos/{}/actions/artifacts?per_page=100",
                self.repo
            ))?
            .iter()
            .map(artifact_of)
            .filter(|a| a.name.starts_with(prefix) && !a.expired)
            .collect())
    }
    fn download_artifact(&self, a: &ArtifactFact, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let status = Command::new("gh")
            .args([
                "run",
                "download",
                &a.run_id.to_string(),
                "--repo",
                &self.repo,
                "-n",
                &a.name,
                "-D",
            ])
            .arg(dir)
            .status()?;
        ensure!(status.success(), "downloading artifact {} failed", a.name);
        Ok(())
    }
    fn public_download(&self, tag: &str, name: &str, dest: &Path) -> Result<()> {
        let url = format!(
            "https://github.com/{}/releases/download/{tag}/{name}",
            self.repo
        );
        let status = Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--retry",
                "3",
                "-o",
            ])
            .arg(dest)
            .arg(&url)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .status()
            .context("running curl")?;
        ensure!(status.success(), "public download of {url} failed");
        Ok(())
    }
    fn authenticate_cell(
        &self,
        evidence: &crate::evidence::schema::Evidence,
        file: &Path,
    ) -> Result<bool> {
        let run = &evidence.run;
        let job = self.api(
            "GET",
            &format!("repos/{}/actions/jobs/{}", self.repo, run.job_id),
            None,
        )?;
        if job["run_id"].as_u64() != Some(run.run_id)
            || job["run_attempt"].as_u64() != Some(u64::from(run.attempt))
            || job["conclusion"] != "success"
        {
            return Ok(false);
        }
        let remote = self.api(
            "GET",
            &format!("repos/{}/actions/runs/{}", self.repo, run.run_id),
            None,
        )?;
        if remote["head_branch"] != "main"
            || remote["head_sha"] != evidence.commit
            || remote["path"] != ".github/workflows/candidate.yml"
            || remote["repository"]["id"]
                .as_u64()
                .map(|id| id.to_string())
                .as_deref()
                != Some(run.repository_id.as_str())
        {
            return Ok(false);
        }
        let artifact = self.api(
            "GET",
            &format!("repos/{}/actions/artifacts/{}", self.repo, run.artifact_id),
            None,
        )?;
        if artifact["expired"] != false
            || artifact["workflow_run"]["id"].as_u64() != Some(run.run_id)
            || artifact["digest"] != format!("sha256:{}", run.artifact_service_digest)
        {
            return Ok(false);
        }
        let name = artifact["name"].as_str().context("artifact has no name")?;
        if !name.starts_with("evidence-candidate-") {
            return Ok(false);
        }
        let fact = ArtifactFact {
            id: run.artifact_id,
            name: name.into(),
            run_id: run.run_id,
            head_branch: "main".into(),
            expired: false,
            digest: Some(format!("sha256:{}", run.artifact_service_digest)),
        };
        let dir = tempfile::tempdir()?;
        self.download_artifact(&fact, dir.path())?;
        let expected_hash = crate::evidence::digest::sha256_file(file)?;
        let mut pending = vec![dir.path().to_owned()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push(entry.path());
                } else if entry.file_name() == "evidence.json"
                    && crate::evidence::digest::sha256_file(&entry.path())? == expected_hash
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    fn verify_attestation(&self, path: &Path) -> Result<bool> {
        let status = Command::new("gh")
            .args(["attestation", "verify"])
            .arg(path)
            .args(["--repo", &self.repo, "--signer-workflow"])
            .arg(format!("{}/.github/workflows/candidate.yml", self.repo))
            .args(["--source-ref", "refs/heads/main"])
            .stdout(Stdio::null())
            .status()
            .context("running gh attestation verify")?;
        Ok(status.success())
    }
}

// ── Directory store ──────────────────────────────────────────────────────────

/// Layout under the store root:
/// `main` (head sha), `ancestors` (one sha per line), `repository-id`,
/// `next-id`, `releases/<id>/release.json` and `releases/<id>/assets/<name>`,
/// `tags/<name>` (commit), `runs/<workflow>.json`, `artifacts/<id>/meta.json`
/// with `artifacts/<id>/files/`, `public/<tag>/<name>` (written on publish),
/// and `faults`: one-shot injected failures, one `op:arg` per line.
pub struct DirHub {
    root: PathBuf,
    repo: String,
}

impl DirHub {
    pub fn open(root: &Path, repo: &str) -> Result<DirHub> {
        ensure!(root.is_dir(), "store {} does not exist", root.display());
        Ok(DirHub {
            root: root.to_path_buf(),
            repo: repo.into(),
        })
    }
    fn next_id(&self) -> Result<u64> {
        let p = self.root.join("next-id");
        let n: u64 = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1000);
        std::fs::write(&p, (n + 1).to_string())?;
        Ok(n)
    }
    /// Fail once if `op` is listed in `faults` (then remove it).
    fn fault(&self, op: &str) -> Result<()> {
        let p = self.root.join("faults");
        let Ok(text) = std::fs::read_to_string(&p) else {
            return Ok(());
        };
        let mut lines: Vec<&str> = text.lines().collect();
        if let Some(i) = lines.iter().position(|l| l.trim() == op) {
            lines.remove(i);
            std::fs::write(&p, lines.join("\n"))?;
            bail!("injected fault: {op}");
        }
        Ok(())
    }
    fn release_dir(&self, id: u64) -> PathBuf {
        self.root.join("releases").join(id.to_string())
    }
    fn load(&self, id: u64) -> Result<ReleaseFact> {
        let dir = self.release_dir(id);
        let mut r: ReleaseFact = serde_json::from_slice(
            &std::fs::read(dir.join("release.json")).with_context(|| format!("release {id}"))?,
        )?;
        r.assets.clear();
        if let Ok(entries) = std::fs::read_dir(dir.join("assets")) {
            let mut names: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            names.sort();
            for path in names {
                let file = path.file_name().unwrap().to_string_lossy().to_string();
                let Some((id, name)) = file.split_once('-') else {
                    continue;
                };
                r.assets.push(AssetFact {
                    id: id.parse()?,
                    name: name.into(),
                    size: std::fs::metadata(&path)?.len(),
                    digest: Some(format!(
                        "sha256:{}",
                        crate::evidence::digest::sha256_file(&path)?
                    )),
                });
            }
        }
        Ok(r)
    }
    fn save(&self, r: &ReleaseFact) -> Result<()> {
        let dir = self.release_dir(r.id);
        std::fs::create_dir_all(dir.join("assets"))?;
        let mut bare = r.clone();
        bare.assets.clear();
        std::fs::write(dir.join("release.json"), serde_json::to_vec_pretty(&bare)?)?;
        Ok(())
    }
    fn tag_path(&self, name: &str) -> PathBuf {
        self.root.join("tags").join(name.replace('/', "%2F"))
    }
    fn asset_path(&self, asset: u64) -> Result<PathBuf> {
        for entry in std::fs::read_dir(self.root.join("releases"))?.flatten() {
            if let Ok(files) = std::fs::read_dir(entry.path().join("assets")) {
                for f in files.flatten() {
                    if f.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{asset}-"))
                    {
                        return Ok(f.path());
                    }
                }
            }
        }
        bail!("no asset {asset}")
    }
}

impl Hub for DirHub {
    fn repo(&self) -> &str {
        &self.repo
    }
    fn repository_id(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.root.join("repository-id"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| "local".into()))
    }
    fn releases(&self) -> Result<Vec<ReleaseFact>> {
        self.fault("releases")?;
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.root.join("releases")) {
            for e in entries.flatten() {
                if let Ok(id) = e.file_name().to_string_lossy().parse::<u64>() {
                    out.push(self.load(id)?);
                }
            }
        }
        out.sort_by_key(|r| r.id);
        Ok(out)
    }
    fn create_draft(&self, tag: &str, name: &str, commit: &str, body: &str) -> Result<ReleaseFact> {
        self.fault("create_draft")?;
        let r = ReleaseFact {
            id: self.next_id()?,
            tag: tag.into(),
            name: name.into(),
            draft: true,
            target_commitish: commit.into(),
            body: body.into(),
            url: format!("store://releases/{tag}"),
            assets: Vec::new(),
        };
        self.save(&r)?;
        Ok(r)
    }
    fn edit_release(&self, id: u64, edit: &ReleaseEdit) -> Result<ReleaseFact> {
        let mut r = self.load(id)?;
        if edit.draft == Some(false) && r.draft {
            self.fault("publish")?;
            // Publishing creates the tag if absent, as GitHub does.
            if self.tag(&r.tag)?.is_none() {
                std::fs::create_dir_all(self.root.join("tags"))?;
                std::fs::write(self.tag_path(&r.tag), &r.target_commitish)?;
            }
            let public = self.root.join("public").join(r.tag.replace('/', "%2F"));
            std::fs::create_dir_all(&public)?;
            for a in &r.assets {
                std::fs::copy(self.asset_path(a.id)?, public.join(&a.name))?;
            }
        }
        if let Some(d) = edit.draft {
            r.draft = d;
        }
        if let Some(n) = &edit.name {
            r.name = n.clone();
        }
        if let Some(b) = &edit.body {
            r.body = b.clone();
        }
        self.save(&r)?;
        self.load(id)
    }
    fn delete_release(&self, id: u64) -> Result<()> {
        self.fault("delete_release")?;
        std::fs::remove_dir_all(self.release_dir(id)).with_context(|| format!("release {id}"))
    }
    fn upload_asset(&self, release: u64, name: &str, path: &Path) -> Result<AssetFact> {
        self.fault(&format!("upload:{name}"))?;
        let r = self.load(release)?;
        ensure!(r.draft, "assets of a published release are immutable");
        ensure!(
            r.asset(name).is_none(),
            "asset {name} already exists (HTTP 422)"
        );
        let id = self.next_id()?;
        std::fs::copy(
            path,
            self.release_dir(release)
                .join("assets")
                .join(format!("{id}-{name}")),
        )?;
        self.load(release)?
            .asset(name)
            .cloned()
            .context("uploaded asset vanished")
    }
    fn delete_asset(&self, asset: u64) -> Result<()> {
        Ok(std::fs::remove_file(self.asset_path(asset)?)?)
    }
    fn download_asset(&self, asset: u64, dest: &Path) -> Result<()> {
        std::fs::copy(self.asset_path(asset)?, dest)?;
        Ok(())
    }
    fn tag(&self, name: &str) -> Result<Option<String>> {
        Ok(std::fs::read_to_string(self.tag_path(name))
            .ok()
            .map(|s| s.trim().to_owned()))
    }
    fn create_tag(&self, name: &str, commit: &str, _message: &str) -> Result<()> {
        self.fault("create_tag")?;
        ensure!(
            self.tag(name)?.is_none(),
            "reference already exists (HTTP 422)"
        );
        std::fs::create_dir_all(self.root.join("tags"))?;
        std::fs::write(self.tag_path(name), commit)?;
        Ok(())
    }
    fn policy_checkout(
        &self,
        checkout: &Path,
        _out: &Path,
        _version: &str,
    ) -> Result<(String, PathBuf)> {
        let live = self.root.join("main-policy");
        Ok((
            self.main_head()?,
            if live.is_dir() {
                live
            } else {
                checkout.to_owned()
            },
        ))
    }
    fn main_head(&self) -> Result<String> {
        self.fault("main_head")?;
        Ok(std::fs::read_to_string(self.root.join("main"))
            .context("store has no main")?
            .trim()
            .to_owned())
    }
    fn is_ancestor_of_main(&self, commit: &str) -> Result<bool> {
        let head = self.main_head()?;
        let ancestors = std::fs::read_to_string(self.root.join("ancestors")).unwrap_or_default();
        Ok(head == commit || ancestors.lines().any(|l| l.trim() == commit))
    }
    fn runs(&self, workflow: &str) -> Result<Vec<RunFact>> {
        match std::fs::read(self.root.join("runs").join(format!("{workflow}.json"))) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(_) => Ok(Vec::new()),
        }
    }
    fn artifacts(&self, prefix: &str) -> Result<Vec<ArtifactFact>> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.root.join("artifacts")) {
            for e in entries.flatten() {
                let a: ArtifactFact =
                    serde_json::from_slice(&std::fs::read(e.path().join("meta.json"))?)?;
                if a.name.starts_with(prefix) && !a.expired {
                    out.push(a);
                }
            }
        }
        out.sort_by_key(|a| a.id);
        Ok(out)
    }
    fn download_artifact(&self, a: &ArtifactFact, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let src = self
            .root
            .join("artifacts")
            .join(a.id.to_string())
            .join("files");
        for f in std::fs::read_dir(&src)?.flatten() {
            std::fs::copy(f.path(), dir.join(f.file_name()))?;
        }
        Ok(())
    }
    fn public_download(&self, tag: &str, name: &str, dest: &Path) -> Result<()> {
        let p = self
            .root
            .join("public")
            .join(tag.replace('/', "%2F"))
            .join(name);
        ensure!(p.exists(), "public download of {tag}/{name} failed (404)");
        std::fs::copy(p, dest)?;
        Ok(())
    }
    fn authenticate_cell(
        &self,
        evidence: &crate::evidence::schema::Evidence,
        file: &Path,
    ) -> Result<bool> {
        let service = self.root.join("service-producers").join(format!(
            "{}-{}-{}",
            evidence.run.run_id, evidence.run.attempt, evidence.run.job_id
        ));
        Ok(std::fs::read_to_string(service).ok().is_some_and(|lines| {
            lines.lines().any(|line| {
                Some(line) == crate::evidence::digest::sha256_file(file).ok().as_deref()
            })
        }))
    }
    fn verify_attestation(&self, path: &Path) -> Result<bool> {
        let sha = crate::evidence::digest::sha256_file(path)?;
        Ok(self.root.join("attestations").join(sha).exists())
    }
}

/// Store an artifact in a [`DirHub`] the way a workflow's upload step would.
/// Used by the pipeline steps' local mode and by the tests.
pub fn dir_put_artifact(root: &Path, name: &str, run_id: u64, files: &[PathBuf]) -> Result<()> {
    let id = {
        let p = root.join("next-id");
        let n: u64 = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1000);
        std::fs::write(&p, (n + 1).to_string())?;
        n
    };
    let dir = root.join("artifacts").join(id.to_string());
    std::fs::create_dir_all(dir.join("files"))?;
    for f in files {
        std::fs::copy(
            f,
            dir.join("files").join(f.file_name().context("file name")?),
        )?;
    }
    let meta = ArtifactFact {
        id,
        name: name.into(),
        run_id,
        head_branch: "main".into(),
        expired: false,
        digest: None,
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_objects_parse() {
        let r = release_of(
            &json!({"id": 5, "tag_name": "v1.0.0", "name": "v1.0.0 (candidate 2)",
            "draft": true, "target_commitish": "abc", "body": "b", "html_url": "u",
            "assets": [{"id": 9, "name": "SHA256SUMS", "size": 10, "digest": "sha256:00"}]}),
        );
        assert_eq!(r.id, 5);
        assert!(r.draft);
        assert_eq!(r.asset("SHA256SUMS").unwrap().id, 9);
        let a = artifact_of(&json!({"id": 3, "name": "x", "expired": false,
            "workflow_run": {"id": 77, "head_branch": "main"}}));
        assert_eq!((a.run_id, a.head_branch.as_str()), (77, "main"));
    }

    #[test]
    fn not_found_is_distinguished_from_other_errors() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let out = |e: &str| std::process::Output {
                status: std::process::ExitStatus::from_raw(256),
                stdout: Vec::new(),
                stderr: e.as_bytes().to_vec(),
            };
            assert!(is_not_found(
                &parse_api("p", out("gh: Not Found (HTTP 404)")).unwrap_err()
            ));
            assert!(!is_not_found(&parse_api("p", out("HTTP 502")).unwrap_err()));
        }
    }
}
