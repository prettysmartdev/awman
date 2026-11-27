//! Content keys for the images awman builds — Layer 0.
//!
//! An image is identified by what it was built from (WI 0163). This module
//! computes that identity as a SHA-256 "inputs hash" and the repository key
//! images are grouped under, and names the labels both are stored in:
//!
//! - **project image**: the bytes of the project Dockerfile (`Dockerfile.dev`
//!   or the configured override) plus every file its `COPY`/`ADD`
//!   instructions name inside the build context, where those can be resolved
//!   statically (see [`resolve_copy_sources`] for the rules). A repository
//!   without a project Dockerfile hashes awman's default base Dockerfile, so
//!   all such repositories share one project image.
//! - **agent image**: the bytes of `.awman/Dockerfile.<agent>` plus the id of
//!   the project image it is layered on, so a new project image gives new
//!   agent images.
//! - **imported image** (`runtime.builtin.imageSource` registry or archive):
//!   the source digest.
//!
//! The hashes depend only on bytes and context-relative paths: never on
//! modification times, on the order the build context lists files in, or on
//! where the repository is checked out. A renamed copied file changes the
//! hash. Content keying cannot see what a build fetches at build time
//! (`curl … | sh`, unpinned `apt-get`): such a Dockerfile hashes the same
//! however different its results.
//!
//! Filesystem access sits behind [`BuildContext`]; [`DirBuildContext`] reads
//! a real directory and [`MemoryBuildContext`] serves tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data::fs::hash::sha256_hex;
use crate::data::image_tags::repo_hash;

/// Image label carrying the [`InputsHash`] an image was built or imported
/// from.
pub const INPUTS_LABEL: &str = "dev.awman.inputs";

/// Image label carrying the [`RepoKey`] of the repository an image was built
/// for.
pub const REPO_LABEL: &str = "dev.awman.repo";

/// Version of the hashing scheme. Bumping it changes every hash, so images
/// built under an older scheme are rebuilt once.
const SCHEME: &str = "awman-inputs-v2";

/// Most files one directory source may contribute before it is treated as
/// unresolved.
pub const MAX_DIR_FILES: usize = 10_000;

/// Most bytes all resolved sources together may contribute before further
/// sources are treated as unresolved.
pub const MAX_CONTEXT_BYTES: u64 = 512 * 1024 * 1024;

/// Hex digits of an [`InputsHash`] used in content-keyed tags.
pub const SHORT_HASH_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Inputs hash
// ---------------------------------------------------------------------------

/// The value of the [`INPUTS_LABEL`] label.
///
/// For built images: 64 lowercase hex digits. For imported images: the source
/// digest as given (`sha256:<hex>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputsHash(String);

impl InputsHash {
    /// Wrap a label value read back from an image.
    pub fn from_label(value: impl Into<String>) -> Self {
        Self(value.into().trim().to_string())
    }

    /// The imported image's hash: its source digest, lowercased.
    pub fn from_source_digest(digest: &str) -> Self {
        Self(digest.trim().to_ascii_lowercase())
    }

    /// The full label value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first [`SHORT_HASH_LEN`] hex digits (after any `algo:` prefix),
    /// for image tags.
    pub fn short(&self) -> String {
        let hex = self.0.rsplit(':').next().unwrap_or(&self.0);
        hex.chars()
            .filter(|c| c.is_ascii_hexdigit())
            .map(|c| c.to_ascii_lowercase())
            .take(SHORT_HASH_LEN)
            .collect()
    }

    /// A non-shareable build still has a stable key within its owning lease.
    pub fn scoped_to_lease(&self, lease: &str) -> Self {
        Self(sha256_hex(format!("{SCHEME}\nlease {lease}\n{}", self.0)))
    }

    /// Whether an image's `dev.awman.inputs` label (absent = `None`) matches.
    /// An unlabeled image never matches, so existing installs converge once.
    pub fn matches_label(&self, label: Option<&str>) -> bool {
        label.is_some_and(|l| l.trim() == self.0)
    }
}

impl fmt::Display for InputsHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The project image's inputs hash and what went into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInputs {
    pub hash: InputsHash,
    /// `false` when the repository has no project Dockerfile and awman's
    /// default base was hashed instead.
    pub dockerfile_present: bool,
    /// Context-relative paths of the files hashed alongside the Dockerfile,
    /// sorted.
    pub files: Vec<String>,
    /// `COPY`/`ADD` sources that could not be resolved statically. Only the
    /// Dockerfile text covers them.
    pub unresolved: Vec<UnresolvedSource>,
}

/// A `COPY`/`ADD` source left out of the hash, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedSource {
    pub source: String,
    pub reason: UnresolvedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// Contains a wildcard (`*`, `?`, `[`).
    Glob,
    /// Contains a `$` variable reference.
    Variable,
    /// A URL or Git remote (`ADD https://…`).
    Remote,
    /// Names a path outside the build context (`../x`).
    OutsideContext,
    /// Names the whole build context (`.`): the image would depend on every
    /// file in the repository, so only the Dockerfile is hashed.
    WholeContext,
    /// A directory larger than [`MAX_DIR_FILES`] files, or the sources
    /// together exceed [`MAX_CONTEXT_BYTES`].
    TooLarge,
    /// The build context could not read it.
    Unreadable,
}

/// Hash a project image's inputs.
///
/// `dockerfile` is the project Dockerfile's bytes, or `None` when the
/// repository has none (awman's default base is hashed). `context` is the
/// build context the Dockerfile's `COPY`/`ADD` sources resolve against (the
/// git root). Never fails: anything it cannot resolve is reported in
/// [`ProjectInputs::unresolved`] and covered only by the Dockerfile text.
pub fn project_inputs(dockerfile: Option<&[u8]>, context: &dyn BuildContext) -> ProjectInputs {
    project_inputs_at(dockerfile, context, "Dockerfile.dev")
}

/// Include both ignore policies; hashing an inactive policy may rebuild extra,
/// but changing which policy Docker selects can never leave the image stale.
pub fn project_inputs_at(
    dockerfile: Option<&[u8]>,
    context: &dyn BuildContext,
    dockerfile_path: &str,
) -> ProjectInputs {
    let dockerfile_path = dockerfile_path.replace('\\', "/");
    let dockerfile_present = dockerfile.is_some();
    let bytes: &[u8] =
        dockerfile.unwrap_or_else(|| crate::data::templates::project_dockerfile_dev().as_bytes());

    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    let mut unresolved = Vec::new();
    let mut budget = MAX_CONTEXT_BYTES;
    let text = String::from_utf8_lossy(bytes);
    let sources = resolve_copy_sources(&text);
    if !sources.is_empty() {
        for rel in [
            ".dockerignore".to_string(),
            format!("{dockerfile_path}.dockerignore"),
        ] {
            if let Err(reason) = collect(context, &rel, &mut entries, &mut budget) {
                unresolved.push(UnresolvedSource {
                    source: rel,
                    reason,
                });
            }
        }
    }
    for source in sources {
        match source {
            CopySource::Path(rel) => {
                let mut found = BTreeMap::new();
                match collect(context, &rel, &mut found, &mut budget) {
                    Ok(()) => entries.extend(found),
                    Err(reason) => unresolved.push(UnresolvedSource {
                        source: rel,
                        reason,
                    }),
                }
            }
            CopySource::Unresolved(u) => unresolved.push(u),
        }
    }

    let mut manifest = format!("{SCHEME}\nproject\ndockerfile {}\n", sha256_hex(bytes));
    for (path, entry) in &entries {
        let path_json = serde_json::to_string(path).expect("string serializes");
        let line = match entry {
            Entry::File(digest, executable) => format!("file {path_json} {digest} {executable}\n"),
            Entry::Symlink(target) => format!(
                "symlink {path_json} {}\n",
                serde_json::to_string(target).expect("string serializes")
            ),
            Entry::Missing => format!("missing {path_json}\n"),
        };
        manifest.push_str(&line);
    }
    let files = entries
        .iter()
        .filter(|(_, e)| !matches!(e, Entry::Missing))
        .map(|(p, _)| p.clone())
        .collect();
    ProjectInputs {
        hash: InputsHash(sha256_hex(manifest)),
        dockerfile_present,
        files,
        unresolved,
    }
}

/// Hash an agent image's inputs: its `.awman/Dockerfile.<agent>` bytes
/// (including one lease provisioning generated) and the id of the project
/// image it is built on.
pub fn agent_inputs_hash(agent_dockerfile: &[u8], project_image_id: &str) -> InputsHash {
    let manifest = format!(
        "{SCHEME}\nagent\ndockerfile {}\nbase {}\n",
        sha256_hex(agent_dockerfile),
        serde_json::to_string(project_image_id.trim()).expect("string serializes"),
    );
    InputsHash(sha256_hex(manifest))
}

/// [`agent_inputs_hash`] plus the agent recipe's own build-context inputs:
/// the files its `COPY`/`ADD` sources name and the ignore policies that
/// select them (the context-root `.dockerignore` and the recipe-specific
/// `<recipe>.dockerignore`), resolved against `context` as for the project
/// image. A recipe without local sources hashes exactly as
/// [`agent_inputs_hash`], so existing images are not rebuilt needlessly.
pub fn agent_inputs_hash_at(
    agent_dockerfile: &[u8],
    project_image_id: &str,
    context: &dyn BuildContext,
    dockerfile_path: &str,
) -> InputsHash {
    let base = agent_inputs_hash(agent_dockerfile, project_image_id);
    let text = String::from_utf8_lossy(agent_dockerfile);
    if !resolve_copy_sources(&text)
        .iter()
        .any(|source| matches!(source, CopySource::Path(_)))
    {
        return base;
    }
    let local = project_inputs_at(Some(agent_dockerfile), context, dockerfile_path).hash;
    InputsHash(sha256_hex(format!(
        "{SCHEME}\nagent-local\nbase {base}\nlocal {local}\n"
    )))
}

/// [`agent_inputs_hash_at`] for the agent recipe `.awman/Dockerfile.<agent>`
/// of the build context rooted at `root`.
pub fn agent_inputs_hash_in(
    root: &std::path::Path,
    agent: &str,
    agent_dockerfile: &[u8],
    project_image_id: &str,
) -> InputsHash {
    agent_inputs_hash_at(
        agent_dockerfile,
        project_image_id,
        &DirBuildContext::new(root),
        &format!(".awman/Dockerfile.{agent}"),
    )
}

/// An imported image's inputs hash: the source digest.
pub fn imported_inputs_hash(source_digest: &str) -> InputsHash {
    InputsHash::from_source_digest(source_digest)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File(String, bool),
    Symlink(String),
    Missing,
}

/// Add `rel` (a file, a symlink, or a directory walked recursively) to
/// `out`. A missing source is recorded as missing so creating it changes the
/// hash.
fn collect(
    context: &dyn BuildContext,
    rel: &str,
    out: &mut BTreeMap<String, Entry>,
    budget: &mut u64,
) -> Result<(), UnresolvedReason> {
    match context.kind(rel) {
        ContextEntry::Missing => {
            out.insert(rel.to_string(), Entry::Missing);
            Ok(())
        }
        ContextEntry::Symlink(target) => {
            out.insert(rel.to_string(), Entry::Symlink(target));
            Ok(())
        }
        ContextEntry::File => {
            let (digest, executable) = context.digest(rel, budget)?;
            out.insert(rel.to_string(), Entry::File(digest, executable));
            Ok(())
        }
        ContextEntry::Dir => {
            let mut stack = vec![rel.to_string()];
            let mut files = 0usize;
            while let Some(dir) = stack.pop() {
                let mut names = context
                    .list(&dir)
                    .map_err(|_| UnresolvedReason::Unreadable)?;
                names.sort();
                for name in names {
                    if name == ".git" || name.is_empty() || name.contains('/') {
                        continue;
                    }
                    let child = format!("{dir}/{name}");
                    match context.kind(&child) {
                        ContextEntry::Dir => stack.push(child),
                        ContextEntry::Missing => {}
                        ContextEntry::Symlink(target) => {
                            files += 1;
                            out.insert(child, Entry::Symlink(target));
                        }
                        ContextEntry::File => {
                            files += 1;
                            let (digest, executable) = context.digest(&child, budget)?;
                            out.insert(child, Entry::File(digest, executable));
                        }
                    }
                    if files > MAX_DIR_FILES {
                        return Err(UnresolvedReason::TooLarge);
                    }
                }
            }
            Ok(())
        }
    }
}

fn take_budget(budget: &mut u64, n: u64) -> Result<(), UnresolvedReason> {
    *budget = budget.checked_sub(n).ok_or(UnresolvedReason::TooLarge)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dockerfile COPY/ADD resolution
// ---------------------------------------------------------------------------

/// One `COPY`/`ADD` source from a Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopySource {
    /// A normalized, context-relative path (`/`-separated, no `.`/`..`, no
    /// leading `/`).
    Path(String),
    Unresolved(UnresolvedSource),
}

/// The context sources a Dockerfile's `COPY`/`ADD` instructions name, in
/// file order.
///
/// Rules:
/// - Instructions are matched case-insensitively after joining `\`
///   continuation lines (or the `# escape=` character) and dropping comment
///   lines.
/// - `COPY --from=…` copies from a stage or image, not the context: skipped.
/// - Flags (`--chown`, `--chmod`, `--link`, `--parents`, `--exclude`,
///   `--checksum`, `--keep-git-dir`, …) are ignored. `--exclude` can only
///   make the hash cover more than the build uses.
/// - Both shell form and JSON-array form are read; the last argument is the
///   destination. Heredoc sources (`<<EOF`) live in the Dockerfile itself.
/// - Unresolved, covered only by the Dockerfile text: wildcards, `$`
///   variables, URLs and Git remotes, paths escaping the context, and the
///   whole context (`.`).
/// - Ignore-file bytes are hashed, but their patterns are not applied. `.git`
///   directories are skipped. Excluded files may cause extra rebuilds.
pub fn resolve_copy_sources(dockerfile: &str) -> Vec<CopySource> {
    let escape = escape_char(dockerfile);
    let mut out = Vec::new();
    for instruction in logical_lines(dockerfile, escape) {
        let trimmed = instruction.trim_start();
        let (keyword, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((k, r)) => (k, r.trim()),
            None => continue,
        };
        if !keyword.eq_ignore_ascii_case("COPY") && !keyword.eq_ignore_ascii_case("ADD") {
            continue;
        }
        let Some(args) = instruction_args(rest, escape) else {
            out.push(CopySource::Unresolved(UnresolvedSource {
                source: rest.into(),
                reason: UnresolvedReason::Unreadable,
            }));
            continue;
        };
        let mut from_stage = false;
        let mut positional: Vec<String> = Vec::new();
        for arg in args {
            if positional.is_empty() && arg.starts_with("--") {
                if arg.to_ascii_lowercase().starts_with("--from") {
                    from_stage = true;
                }
                continue;
            }
            positional.push(arg);
        }
        if from_stage || positional.len() < 2 {
            continue;
        }
        positional.pop(); // destination
        for source in positional {
            if source.starts_with("<<") {
                continue;
            }
            out.push(classify_source(&source));
        }
    }
    out
}

fn classify_source(source: &str) -> CopySource {
    let unresolved = |reason| {
        CopySource::Unresolved(UnresolvedSource {
            source: source.to_string(),
            reason,
        })
    };
    let lower = source.to_ascii_lowercase();
    if lower.contains("://") || lower.starts_with("git@") {
        return unresolved(UnresolvedReason::Remote);
    }
    if source.contains('$') {
        return unresolved(UnresolvedReason::Variable);
    }
    if source.contains(['*', '?', '[']) {
        return unresolved(UnresolvedReason::Glob);
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in source.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return unresolved(UnresolvedReason::OutsideContext);
                }
            }
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return unresolved(UnresolvedReason::WholeContext);
    }
    CopySource::Path(parts.join("/"))
}

/// The `# escape=` parser directive, `\` by default. Directives are only
/// read before the first non-directive line.
fn escape_char(dockerfile: &str) -> char {
    for line in dockerfile.lines() {
        let Some(body) = line.trim().strip_prefix('#') else {
            break;
        };
        let Some((key, value)) = body.split_once('=') else {
            break;
        };
        if key.trim().eq_ignore_ascii_case("escape") {
            if let Some(c) = value.trim().chars().next() {
                return c;
            }
        }
    }
    '\\'
}

/// Join continuation lines and drop comments and blank lines.
fn logical_lines(dockerfile: &str, escape: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in dockerfile.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(head) = line.trim_end().strip_suffix(escape) {
            current.push_str(head);
            current.push(' ');
            continue;
        }
        current.push_str(line);
        if !current.trim().is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.clear();
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Split an instruction's arguments: leading `--flags` then either a JSON
/// array or whitespace-separated words. `None` when the JSON form is
/// malformed.
fn instruction_args(rest: &str, escape: char) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut rest = rest.trim();
    while rest.starts_with("--") {
        let (flag, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        args.push(flag.to_string());
        rest = tail.trim_start();
    }
    if rest.starts_with('[') {
        let array: Vec<String> = serde_json::from_str(rest).ok()?;
        args.extend(array);
    } else {
        args.extend(shell_words(rest, escape)?);
    }
    Some(args)
}

/// Dockerfile shell words, without expansion. Variables remain unresolved.
fn shell_words(text: &str, escape: char) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == escape && quote != Some('\'') {
            word.push(chars.next()?);
            started = true;
        } else if Some(c) == quote {
            quote = None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
            started = true;
        } else if quote.is_none() && c.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

/// Bind only FROM image tokens, preserving unrelated instructions and comments.
/// ARG defaults are resolved without evaluating shell expressions.
pub fn bind_project_base(
    bytes: &[u8],
    named_base: &str,
    immutable_base: &str,
) -> Result<String, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Dockerfile must be UTF-8")?;
    let escape = escape_char(text);
    let mut args = BTreeMap::<String, String>::new();
    let mut output = String::new();
    let mut raw = String::new();
    let mut project_base = None;
    let mut heredocs = std::collections::VecDeque::<String>::new();
    for line in text.split_inclusive('\n') {
        if let Some(delimiter) = heredocs.front() {
            output.push_str(line);
            if line.trim() == delimiter {
                heredocs.pop_front();
            }
            continue;
        }
        raw.push_str(line);
        if line.trim_end().ends_with(escape) {
            continue;
        }
        let logical = logical_lines(&raw, escape).join(" ");
        let trimmed = logical.trim_start();
        if let Some((keyword, rest)) = trimmed.split_once(char::is_whitespace) {
            if keyword.eq_ignore_ascii_case("ARG") {
                if let Some((key, value)) = rest.trim().split_once('=') {
                    args.insert(key.to_string(), value.trim_matches(['\'', '"']).to_string());
                }
            }
            if keyword.eq_ignore_ascii_case("FROM") {
                let mut words = shell_words(rest, escape).ok_or("unsupported FROM syntax")?;
                let index = words
                    .iter()
                    .position(|w| !w.starts_with("--"))
                    .ok_or("missing FROM image")?;
                let mut base = words[index].clone();
                let mut expanded = String::new();
                let mut chars = base.chars().peekable();
                while let Some(c) = chars.next() {
                    if c != '$' {
                        expanded.push(c);
                        continue;
                    }
                    let braced = chars.peek() == Some(&'{');
                    if braced {
                        chars.next();
                    }
                    let mut key = String::new();
                    while chars
                        .peek()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                    {
                        key.push(chars.next().unwrap());
                    }
                    if braced && chars.next() != Some('}') {
                        return Err("unsupported FROM variable syntax".into());
                    }
                    expanded.push_str(
                        args.get(&key)
                            .ok_or("FROM variable has no supported ARG default")?,
                    );
                }
                base = expanded;
                if base.contains('$') {
                    return Err("FROM variable has no supported ARG default".into());
                }
                if base == named_base || base.starts_with("awman-") {
                    let canonical = base.strip_suffix(":latest").unwrap_or(&base).to_string();
                    if canonical.contains(':') || canonical.contains('@') {
                        return Err(
                            "agent FROM must name an untagged or :latest awman project image"
                                .into(),
                        );
                    }
                    if project_base
                        .as_ref()
                        .is_some_and(|previous| previous != &canonical)
                    {
                        return Err("agent recipe references multiple awman project images".into());
                    }
                    project_base = Some(canonical);
                    words[index] = immutable_base.into();
                    output.push_str(&format!("FROM {}\n", words.join(" ")));
                    raw.clear();
                    continue;
                }
            }
        }
        if trimmed.split_whitespace().next().is_some_and(|keyword| {
            ["RUN", "COPY", "ADD"]
                .iter()
                .any(|known| keyword.eq_ignore_ascii_case(known))
        }) {
            for token in shell_words(trimmed, escape).unwrap_or_default() {
                if let Some(marker) = token
                    .strip_prefix("<<-")
                    .or_else(|| token.strip_prefix("<<"))
                {
                    if !marker.is_empty() {
                        heredocs.push_back(marker.to_string());
                    }
                }
            }
        }
        output.push_str(&raw);
        raw.clear();
    }
    output.push_str(&raw);
    Ok(output)
}

// ---------------------------------------------------------------------------
// Build context seam
// ---------------------------------------------------------------------------

/// What a context-relative path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextEntry {
    File,
    Dir,
    /// A symbolic link, hashed by its target text and never followed.
    Symlink(String),
    Missing,
}

/// Read access to a build context. Paths are `/`-separated and relative to
/// the context root; implementations must never resolve outside it.
pub trait BuildContext {
    fn kind(&self, rel: &str) -> ContextEntry;
    fn read(&self, rel: &str) -> std::io::Result<Vec<u8>>;
    /// Implementations backed by storage must bound reads before allocating.
    fn digest(&self, rel: &str, budget: &mut u64) -> Result<(String, bool), UnresolvedReason> {
        if *budget == 0 {
            return Err(UnresolvedReason::TooLarge);
        }
        let bytes = self.read(rel).map_err(|_| UnresolvedReason::Unreadable)?;
        take_budget(budget, bytes.len() as u64)?;
        Ok((sha256_hex(bytes), false))
    }
    /// The names (not paths) of a directory's entries.
    fn list(&self, rel: &str) -> std::io::Result<Vec<String>>;
}

/// A build context on disk. Symbolic links are reported, never followed, so
/// nothing outside `root` is read.
#[derive(Debug, Clone)]
pub struct DirBuildContext {
    root: PathBuf,
}

impl DirBuildContext {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn segments<'a>(&self, rel: &'a str) -> std::io::Result<Vec<&'a str>> {
        let parts: Vec<_> = rel.split('/').collect();
        if parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\\'))
        {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        Ok(parts)
    }

    #[cfg(unix)]
    pub fn open_file(&self, rel: &str) -> std::io::Result<std::fs::File> {
        crate::data::fs::safe_tree::platform::open_context(&self.root, &self.segments(rel)?)
    }

    // Windows has no std openat. Hold each directory with sharing that denies
    // write/delete while resolving its child: neither renames nor reparse-point
    // changes can redirect that pathname. Inspect the final no-follow handle.
    #[cfg(windows)]
    fn windows_directories(
        &self,
        rel: &str,
        include_last: bool,
    ) -> std::io::Result<(Vec<std::fs::File>, PathBuf)> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        let parts = self.segments(rel)?;
        for part in &parts {
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            let device = matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|tail| {
                    matches!(
                        tail,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            });
            if device || part.contains(':') || part.ends_with(['.', ' ']) {
                return Err(std::io::ErrorKind::InvalidInput.into());
            }
        }
        let root = if self.root.is_absolute() {
            self.root.clone()
        } else {
            std::env::current_dir()?.join(&self.root)
        };
        let mut path = PathBuf::new();
        let mut directories = Vec::new();
        let mut pin = |path: &Path| -> std::io::Result<()> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .custom_flags(0x02000000 | 0x00200000)
                .open(path)?; // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
            let meta = file.metadata()?;
            if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            directories.push(file);
            Ok(())
        };
        for component in root.components() {
            match component {
                std::path::Component::Prefix(_) => {
                    path.push(component);
                    continue;
                }
                std::path::Component::CurDir => continue,
                std::path::Component::ParentDir => {
                    return Err(std::io::ErrorKind::InvalidInput.into())
                }
                _ => path.push(component),
            }
            pin(&path)?;
        }
        for part in &parts[..parts.len() - usize::from(!include_last)] {
            path.push(part);
            pin(&path)?;
        }
        if !include_last {
            path.push(parts.last().unwrap());
        }
        Ok((directories, path))
    }

    #[cfg(windows)]
    pub fn open_file(&self, rel: &str) -> std::io::Result<std::fs::File> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        let (_directories, path) = self.windows_directories(rel, false)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x00200000)
            .open(path)?; // OPEN_REPARSE_POINT
        let meta = file.metadata()?;
        if !meta.is_file() || meta.file_attributes() & 0x400 != 0 {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        Ok(file)
    }

    #[cfg(not(any(unix, windows)))]
    pub fn open_file(&self, _: &str) -> std::io::Result<std::fs::File> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(windows)]
    fn list_secure(&self, rel: &str) -> std::io::Result<Vec<String>> {
        let (_directories, path) = self.windows_directories(rel, true)?;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
            if names.len() > MAX_DIR_FILES {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
        }
        Ok(names)
    }

    #[cfg(unix)]
    fn list_secure(&self, rel: &str) -> std::io::Result<Vec<String>> {
        use nix::{
            fcntl::{openat, OFlag},
            sys::stat::Mode,
        };
        let mut dir = crate::data::fs::safe_tree::platform::context_root(&self.root, false)?;
        for part in self.segments(rel)? {
            dir = openat(
                &dir,
                part,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )?;
        }
        let mut names = Vec::new();
        for entry in nix::dir::Dir::from_fd(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy();
            if name != "." && name != ".." {
                names.push(name.into_owned());
            }
            if names.len() > MAX_DIR_FILES {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
        }
        Ok(names)
    }

    #[cfg(not(any(unix, windows)))]
    fn list_secure(&self, _: &str) -> std::io::Result<Vec<String>> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

impl BuildContext for DirBuildContext {
    fn kind(&self, rel: &str) -> ContextEntry {
        // Every ancestor must be a real directory, not a link out of root.
        let mut at = self.root.clone();
        let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        for (i, part) in parts.iter().enumerate() {
            at.push(part);
            let Ok(meta) = std::fs::symlink_metadata(&at) else {
                return ContextEntry::Missing;
            };
            let last = i + 1 == parts.len();
            if meta.file_type().is_symlink() {
                return if last {
                    let target = std::fs::read_link(&at)
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ContextEntry::Symlink(target)
                } else {
                    ContextEntry::Missing
                };
            }
            if last {
                return if meta.is_dir() {
                    ContextEntry::Dir
                } else if meta.is_file() {
                    ContextEntry::File
                } else {
                    ContextEntry::Missing
                };
            }
            if !meta.is_dir() {
                return ContextEntry::Missing;
            }
        }
        ContextEntry::Dir
    }

    fn read(&self, rel: &str) -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let file = self.open_file(rel)?;
        if file.metadata()?.len() > MAX_CONTEXT_BYTES {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONTEXT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONTEXT_BYTES {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        Ok(bytes)
    }

    fn digest(&self, rel: &str, budget: &mut u64) -> Result<(String, bool), UnresolvedReason> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        if *budget == 0 {
            return Err(UnresolvedReason::TooLarge);
        }
        let mut file = self
            .open_file(rel)
            .map_err(|_| UnresolvedReason::Unreadable)?;
        let meta = file.metadata().map_err(|_| UnresolvedReason::Unreadable)?;
        if meta.len() > *budget {
            return Err(UnresolvedReason::TooLarge);
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let limit = buffer.len().min(budget.saturating_add(1) as usize);
            let n = file
                .read(&mut buffer[..limit])
                .map_err(|_| UnresolvedReason::Unreadable)?;
            if n == 0 {
                break;
            }
            take_budget(budget, n as u64)?;
            hash.update(&buffer[..n]);
        }
        Ok((
            hash.finalize().iter().map(|b| format!("{b:02x}")).collect(),
            executable,
        ))
    }

    fn list(&self, rel: &str) -> std::io::Result<Vec<String>> {
        self.list_secure(rel)
    }
}

/// An in-memory build context: `/`-separated paths to file bytes.
/// Directories are implied by their files.
#[derive(Debug, Clone, Default)]
pub struct MemoryBuildContext {
    files: BTreeMap<String, Vec<u8>>,
}

impl MemoryBuildContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(mut self, path: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.insert(path, bytes);
        self
    }

    pub fn insert(&mut self, path: &str, bytes: impl Into<Vec<u8>>) {
        self.files
            .insert(path.trim_matches('/').to_string(), bytes.into());
    }
}

impl BuildContext for MemoryBuildContext {
    fn kind(&self, rel: &str) -> ContextEntry {
        if self.files.contains_key(rel) {
            return ContextEntry::File;
        }
        let prefix = format!("{rel}/");
        if self.files.keys().any(|k| k.starts_with(&prefix)) {
            ContextEntry::Dir
        } else {
            ContextEntry::Missing
        }
    }

    fn read(&self, rel: &str) -> std::io::Result<Vec<u8>> {
        self.files
            .get(rel)
            .cloned()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    fn list(&self, rel: &str) -> std::io::Result<Vec<String>> {
        let prefix = format!("{rel}/");
        let names: BTreeSet<String> = self
            .files
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix))
            .filter_map(|tail| tail.split('/').next())
            .map(str::to_string)
            .collect();
        Ok(names.into_iter().collect())
    }
}

// ---------------------------------------------------------------------------
// Repository key
// ---------------------------------------------------------------------------

/// Hex digits of the disambiguating hash in [`RepoKey::tag_component`].
const REPO_TAG_HASH_LEN: usize = 8;
/// Longest readable part of [`RepoKey::tag_component`].
const REPO_TAG_READABLE_MAX: usize = 40;

/// The repository images are grouped under: the value of [`REPO_LABEL`].
///
/// For a remote, the origin URL reduced to `host[:port]/path` with the
/// scheme, user info, password, query, fragment and a trailing `.git`
/// removed, so `https://user:token@github.com/org/x.git`,
/// `ssh://git@github.com/org/x` and `git@github.com:org/x.git` all give
/// `github.com/org/x`. For a local repository (no usable origin),
/// [`repo_hash`] of the git root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoKey(String);

impl RepoKey {
    /// The key for a repository with `origin` (the remote URL, if any) at
    /// `git_root`. Falls back to the local key when the origin is absent,
    /// a local path or a `file:` URL.
    pub fn for_repo(origin: Option<&str>, git_root: &Path) -> Self {
        origin
            .and_then(Self::from_remote_url)
            .unwrap_or_else(|| Self::local(git_root))
    }

    /// Leases of a local origin share the origin's identity, not their clone paths.
    pub fn for_lease(url: &str, git_root: &Path) -> Self {
        if let Some(remote) = Self::from_remote_url(url) {
            return remote;
        }
        let path = url.strip_prefix("file://").unwrap_or(url);
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        if path.is_absolute() {
            Self::local(&path)
        } else {
            Self::local(git_root)
        }
    }

    /// The key for a remote URL, or `None` when it names no network host.
    pub fn from_remote_url(url: &str) -> Option<Self> {
        sanitize_remote(url).map(Self)
    }

    /// The key for a repository with no remote: [`repo_hash`] of its root.
    pub fn local(git_root: &Path) -> Self {
        Self(repo_hash(git_root))
    }

    /// Wrap a label value read back from an image.
    pub fn from_label(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The label value. Never carries credentials.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A valid OCI repository-name component for this key: a readable,
    /// length-capped form of the key plus an 8-hex hash of the full key, so
    /// keys that read alike (`a/b-c` and `a/b/c`) never share tags.
    pub fn tag_component(&self) -> String {
        let readable = crate::data::image_tags::sanitize_image_name(&self.0);
        let mut readable: String = readable.chars().take(REPO_TAG_READABLE_MAX).collect();
        while readable.ends_with(['.', '_', '-']) {
            readable.pop();
        }
        let hash: String = sha256_hex(&self.0)
            .chars()
            .take(REPO_TAG_HASH_LEN)
            .collect();
        if readable.is_empty() {
            hash
        } else {
            format!("{readable}-{hash}")
        }
    }
}

impl fmt::Display for RepoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `host[:port]/path` for a remote URL; `None` for local paths.
fn sanitize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => {
            // scp-like `[user@]host:path`. A local path has no `:` before
            // its first `/` (or is a Windows drive letter).
            let colon = url.find(':')?;
            if url[..colon].contains('/') || colon <= 1 {
                return None;
            }
            ("ssh".to_string(), url)
        }
    };
    if scheme == "file" {
        return None;
    }
    // Strip query and fragment, which may carry tokens.
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let scp = !url.contains("://");
    let (authority, path) = if scp {
        let (a, p) = rest.split_once(':')?;
        (a, p)
    } else {
        rest.split_once('/').unwrap_or((rest, ""))
    };
    // Drop user info (`user:token@`).
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = split_port(host_port);
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let default_port = match scheme.as_str() {
        "https" => Some("443"),
        "http" => Some("80"),
        "ssh" | "git+ssh" | "ssh+git" => Some("22"),
        "git" => Some("9418"),
        _ => None,
    };
    let mut out = host;
    if let Some(port) = port {
        if Some(port) != default_port && port.chars().all(|c| c.is_ascii_digit()) {
            out.push(':');
            out.push_str(port);
        }
    }
    let mut segments: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    if let Some(last) = segments.last_mut() {
        if let Some(stem) = last.strip_suffix(".git") {
            *last = stem;
        }
    }
    segments.retain(|s| !s.is_empty());
    for seg in segments {
        out.push('/');
        out.push_str(seg);
    }
    if out.contains('@') {
        return None;
    }
    Some(out)
}

/// Split `host:port`, keeping bracketed IPv6 hosts whole.
fn split_port(host_port: &str) -> (&str, Option<&str>) {
    if host_port.starts_with('[') {
        return match host_port.find(']') {
            Some(end) => {
                let port = host_port[end + 1..].strip_prefix(':');
                (&host_port[..=end], port)
            }
            None => (host_port, None),
        };
    }
    match host_port.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_and_escaped_copy_sources_track_the_real_files() {
        for df in [
            "FROM x\nCOPY \"setup.sh\" /setup.sh\n",
            "FROM x\nADD 'setup.sh' /setup.sh\n",
            "FROM x\nCOPY setup\\ file other /dst/\n",
            "# escape=`\nFROM x\nCOPY \"setup file\" `\n other /dst/\n",
        ] {
            let a = MemoryBuildContext::new()
                .with_file("setup.sh", "v1")
                .with_file("setup file", "v1")
                .with_file("other", "other");
            let b = a
                .clone()
                .with_file("setup.sh", "v2")
                .with_file("setup file", "v2");
            let before = project_inputs(Some(df.as_bytes()), &a);
            assert!(before.unresolved.is_empty(), "{df}");
            assert_ne!(
                before.hash,
                project_inputs(Some(df.as_bytes()), &b).hash,
                "{df}"
            );
        }
        assert!(!project_inputs(Some(b"COPY \"broken /dst"), &ctx())
            .unresolved
            .is_empty());
    }

    #[test]
    fn ignore_policy_changes_in_both_directions_invalidate_copy_inputs() {
        let df = b"FROM x\nCOPY tools /tools\n";
        let base = ctx().with_file(".dockerignore", "tools/nested\n");
        let included = base.clone().with_file(".dockerignore", "");
        let a = project_inputs(Some(df), &base).hash;
        let b = project_inputs(Some(df), &included).hash;
        assert_ne!(a, b);
        assert_eq!(a, project_inputs(Some(df), &base).hash);
        let custom = base
            .clone()
            .with_file("build/recipe.dockerignore", "tools\n");
        assert_ne!(
            project_inputs_at(Some(df), &base, "build/recipe").hash,
            project_inputs_at(Some(df), &custom, "build/recipe").hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn swapped_files_and_ancestors_are_refused_at_read_and_enumeration() {
        use std::os::unix::fs::symlink;
        let s = awman_test_support::TestSandbox::new();
        std::fs::create_dir(s.work().join("dir")).unwrap();
        std::fs::write(s.work().join("dir/file"), "inside").unwrap();
        std::fs::write(s.tmp().join("file"), "outside").unwrap();
        let context = DirBuildContext::new(s.work());
        assert_eq!(context.kind("dir/file"), ContextEntry::File);
        std::fs::remove_file(s.work().join("dir/file")).unwrap();
        symlink(s.tmp().join("file"), s.work().join("dir/file")).unwrap();
        assert!(context.read("dir/file").is_err());
        std::fs::remove_file(s.work().join("dir/file")).unwrap();
        std::fs::remove_dir(s.work().join("dir")).unwrap();
        symlink(s.tmp(), s.work().join("dir")).unwrap();
        assert!(context.read("dir/file").is_err());
        assert!(context.list("dir").is_err());
        assert!(context.read("../file").is_err());
    }

    #[test]
    fn disk_digest_enforces_the_remaining_budget_before_reading() {
        let s = awman_test_support::TestSandbox::new();
        let file = std::fs::File::create(s.work().join("large")).unwrap();
        file.set_len(MAX_CONTEXT_BYTES * 8).unwrap();
        drop(file);
        let context = DirBuildContext::new(s.work());
        let mut budget = 3;
        assert_eq!(
            context.digest("large", &mut budget),
            Err(UnresolvedReason::TooLarge)
        );
        assert_eq!(budget, 3);
        std::fs::write(s.work().join("small"), "1234").unwrap();
        assert_eq!(
            context.digest("small", &mut budget),
            Err(UnresolvedReason::TooLarge)
        );
        std::fs::write(s.work().join("small"), "123").unwrap();
        assert!(context.digest("small", &mut budget).is_ok());
        assert_eq!(budget, 0);
        assert_eq!(
            context.digest("missing", &mut budget),
            Err(UnresolvedReason::TooLarge)
        );
    }

    #[test]
    fn agent_base_binding_handles_all_stages_args_and_untagged_images_only() {
        let df = b"# awman-old:latest stays in comments\nARG BASE=awman-old:latest\nFROM golang AS tools\nFROM ${BASE} AS agent\nCOPY --from=tools /tool /tool\nRUN echo awman-old:latest\n";
        let text = bind_project_base(df, "awman-clone:latest", "sha256:pinned").unwrap();
        assert!(text.contains("FROM sha256:pinned AS agent"));
        assert!(text.contains("FROM golang AS tools"));
        assert!(text.contains("# awman-old:latest stays in comments"));
        assert!(text.contains("RUN echo awman-old:latest"));
        assert!(text.contains("COPY --from=tools"));
        assert_eq!(
            bind_project_base(b"FROM awman-old\n", "awman-clone:latest", "sha256:pinned").unwrap(),
            "FROM sha256:pinned\n"
        );
        assert!(bind_project_base(b"FROM awman-old:dev\n", "awman-clone:latest", "id").is_err());
        assert!(bind_project_base(b"FROM $UNKNOWN\n", "awman-clone:latest", "id").is_err());
        assert!(bind_project_base(
            b"ARG BASE=awman-x\nFROM $BASE_SUFFIX\n",
            "awman-clone:latest",
            "id"
        )
        .is_err());
        let heredoc = b"FROM awman-old\nRUN <<EOF\nFROM awman-unrelated:dev\nEOF\n";
        assert!(bind_project_base(heredoc, "awman-clone:latest", "id")
            .unwrap()
            .contains("FROM awman-unrelated:dev\n"));
        assert!(
            bind_project_base(b"FROM awman-a\nFROM awman-b\n", "awman-clone:latest", "id").is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_changes_the_project_hash() {
        use std::os::unix::fs::PermissionsExt;
        let s = awman_test_support::TestSandbox::new();
        let path = s.work().join("setup.sh");
        std::fs::write(&path, "echo setup").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let context = DirBuildContext::new(s.work());
        let before = project_inputs(Some(b"COPY setup.sh /setup.sh"), &context).hash;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(
            before,
            project_inputs(Some(b"COPY setup.sh /setup.sh"), &context).hash
        );
    }

    #[test]
    fn local_lease_origins_reuse_across_clone_paths_and_malformed_userinfo_is_refused() {
        let s = awman_test_support::TestSandbox::new();
        let origin = s.work();
        assert_eq!(
            RepoKey::for_lease(
                &format!("file://{}", origin.display()),
                &s.tmp().join("lease-a")
            ),
            RepoKey::for_lease(&origin.to_string_lossy(), &s.tmp().join("lease-b"))
        );
        assert!(RepoKey::from_remote_url("user:pass@host:path").is_none());
    }
    fn ctx() -> MemoryBuildContext {
        MemoryBuildContext::new()
            .with_file("scripts/setup.sh", "echo setup")
            .with_file("tools/a.txt", "a")
            .with_file("tools/nested/b.txt", "b")
            .with_file("README.md", "readme")
    }

    const DF: &str = "FROM debian\nCOPY scripts/setup.sh /tmp/\nADD tools /opt/tools\n";

    #[test]
    fn project_hash_is_stable_for_identical_inputs_and_lists_resolved_files() {
        let a = project_inputs(Some(DF.as_bytes()), &ctx());
        let b = project_inputs(Some(DF.as_bytes()), &ctx());
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.hash.as_str().len(), 64);
        assert_eq!(
            a.files,
            vec!["scripts/setup.sh", "tools/a.txt", "tools/nested/b.txt"]
        );
        assert!(a.unresolved.is_empty());
        assert!(a.dockerfile_present);
    }

    #[test]
    fn project_hash_ignores_the_order_the_context_lists_files_in() {
        let reversed = MemoryBuildContext::new()
            .with_file("README.md", "readme")
            .with_file("tools/nested/b.txt", "b")
            .with_file("tools/a.txt", "a")
            .with_file("scripts/setup.sh", "echo setup");
        assert_eq!(
            project_inputs(Some(DF.as_bytes()), &ctx()).hash,
            project_inputs(Some(DF.as_bytes()), &reversed).hash
        );
    }

    #[test]
    fn project_hash_ignores_modification_times_on_disk() {
        let a = awman_test_support::TestSandbox::new();
        let b = awman_test_support::TestSandbox::new();
        let (a, b) = (a.work(), b.work());
        for dir in [a.as_path(), b.as_path()] {
            std::fs::create_dir_all(dir.join("tools/nested")).unwrap();
            std::fs::create_dir_all(dir.join("scripts")).unwrap();
            std::fs::write(dir.join("scripts/setup.sh"), "echo setup").unwrap();
            std::fs::write(dir.join("tools/a.txt"), "a").unwrap();
            std::fs::write(dir.join("tools/nested/b.txt"), "b").unwrap();
        }
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        std::fs::File::options()
            .write(true)
            .open(b.join("tools/a.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let ha = project_inputs(Some(DF.as_bytes()), &DirBuildContext::new(&a));
        let hb = project_inputs(Some(DF.as_bytes()), &DirBuildContext::new(&b));
        assert_eq!(ha.hash, hb.hash);
        // The disk context and the in-memory one agree.
        assert_eq!(ha.hash, project_inputs(Some(DF.as_bytes()), &ctx()).hash);
    }

    #[test]
    fn changing_dockerfile_or_a_copied_file_changes_the_project_hash() {
        let base = project_inputs(Some(DF.as_bytes()), &ctx()).hash;
        let edited_df = format!("{DF}RUN true\n");
        assert_ne!(
            base,
            project_inputs(Some(edited_df.as_bytes()), &ctx()).hash
        );
        let edited_copy = ctx().with_file("tools/nested/b.txt", "B");
        assert_ne!(base, project_inputs(Some(DF.as_bytes()), &edited_copy).hash);
        let added = ctx().with_file("tools/c.txt", "c");
        assert_ne!(base, project_inputs(Some(DF.as_bytes()), &added).hash);
    }

    #[test]
    fn files_the_dockerfile_does_not_copy_leave_the_project_hash_alone() {
        let base = project_inputs(Some(DF.as_bytes()), &ctx()).hash;
        let unrelated = ctx()
            .with_file("README.md", "changed")
            .with_file("src/main.rs", "fn main() {}");
        assert_eq!(base, project_inputs(Some(DF.as_bytes()), &unrelated).hash);
    }

    #[test]
    fn renaming_a_copied_file_changes_the_project_hash() {
        let df = "FROM x\nCOPY tools /t\n";
        let a = MemoryBuildContext::new().with_file("tools/a.txt", "same");
        let b = MemoryBuildContext::new().with_file("tools/b.txt", "same");
        assert_ne!(
            project_inputs(Some(df.as_bytes()), &a).hash,
            project_inputs(Some(df.as_bytes()), &b).hash
        );
    }

    #[test]
    fn creating_a_missing_copy_source_changes_the_project_hash() {
        let df = "FROM x\nCOPY later.sh /\n";
        let before = project_inputs(Some(df.as_bytes()), &MemoryBuildContext::new());
        assert!(before.files.is_empty());
        let after = project_inputs(
            Some(df.as_bytes()),
            &MemoryBuildContext::new().with_file("later.sh", ""),
        );
        assert_ne!(before.hash, after.hash);
    }

    #[test]
    fn missing_dockerfile_hashes_the_default_base_and_is_shared() {
        let none_a = project_inputs(None, &ctx());
        let none_b = project_inputs(None, &MemoryBuildContext::new());
        assert!(!none_a.dockerfile_present);
        assert_eq!(none_a.hash, none_b.hash);
        let explicit = crate::data::templates::project_dockerfile_dev().as_bytes();
        assert_eq!(none_a.hash, project_inputs(Some(explicit), &ctx()).hash);
    }

    #[test]
    fn agent_hash_changes_with_its_dockerfile_or_project_image_only() {
        let base = agent_inputs_hash(b"FROM awman-x\nRUN a", "sha256:111");
        assert_eq!(
            base,
            agent_inputs_hash(b"FROM awman-x\nRUN a", "sha256:111")
        );
        assert_ne!(
            base,
            agent_inputs_hash(b"FROM awman-x\nRUN b", "sha256:111")
        );
        assert_ne!(
            base,
            agent_inputs_hash(b"FROM awman-x\nRUN a", "sha256:222")
        );
        // An agent Dockerfile edit does not touch the project hash.
        let project = project_inputs(Some(DF.as_bytes()), &ctx()).hash;
        assert_ne!(project, base);
    }

    #[test]
    fn imported_hash_is_the_source_digest() {
        let h = imported_inputs_hash(" SHA256:ABCDEF0123456789abcdef ");
        assert_eq!(h.as_str(), "sha256:abcdef0123456789abcdef");
        assert_eq!(h.short(), "abcdef0123456789");
        assert!(h.matches_label(Some("sha256:abcdef0123456789abcdef")));
        assert!(!h.matches_label(None));
    }

    #[test]
    fn copy_resolution_skips_stages_heredocs_and_flags_and_reads_json_form() {
        let df = "# escape=\\\n\
                  FROM rust AS build\n\
                  copy --chown=1:1 --chmod=755 a.sh b.sh /bin/\n\
                  COPY --from=build /out /out\n\
                  COPY <<EOF /etc/x\nEOF\n\
                  ADD [\"json dir/file\", \"/dst\"]\n\
                  COPY ./c/../d/e.txt \\\n    /dst/\n\
                  # COPY commented.sh /\n";
        let got = resolve_copy_sources(df);
        let paths: Vec<_> = got
            .iter()
            .filter_map(|s| match s {
                CopySource::Path(p) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, vec!["a.sh", "b.sh", "json dir/file", "d/e.txt"]);
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn copy_sources_that_cannot_be_resolved_statically_are_reported() {
        let df = "FROM x\n\
                  COPY *.txt /\n\
                  COPY $SRC /\n\
                  ADD https://example.com/x.tgz /\n\
                  ADD git@github.com:o/r.git /\n\
                  COPY ../secret /\n\
                  COPY . /app\n";
        let reasons: Vec<_> = resolve_copy_sources(df)
            .into_iter()
            .map(|s| match s {
                CopySource::Unresolved(u) => u.reason,
                CopySource::Path(p) => panic!("resolved {p}"),
            })
            .collect();
        assert_eq!(
            reasons,
            vec![
                UnresolvedReason::Glob,
                UnresolvedReason::Variable,
                UnresolvedReason::Remote,
                UnresolvedReason::Remote,
                UnresolvedReason::OutsideContext,
                UnresolvedReason::WholeContext,
            ]
        );
        let inputs = project_inputs(Some(df.as_bytes()), &ctx());
        assert!(inputs.files.is_empty());
        assert_eq!(inputs.unresolved.len(), 6);
    }

    #[test]
    fn custom_escape_directive_joins_continuations() {
        let df = "# escape=`\nFROM x\nCOPY a.sh `\n  /dst\n";
        assert_eq!(
            resolve_copy_sources(df),
            vec![CopySource::Path("a.sh".into())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn disk_context_never_follows_symlinks_out_of_the_root() {
        let sandbox = awman_test_support::TestSandbox::new();
        let outside = sandbox.tmp();
        std::fs::write(outside.join("secret"), "s").unwrap();
        let root = sandbox.work();
        std::fs::create_dir(root.join("dir")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dir/out")).unwrap();
        let ctx = DirBuildContext::new(&root);
        assert!(matches!(ctx.kind("dir/out"), ContextEntry::Symlink(_)));
        assert_eq!(ctx.kind("dir/out/secret"), ContextEntry::Missing);
        let inputs = project_inputs(Some(b"FROM x\nCOPY dir /d\n"), &ctx);
        assert_eq!(inputs.files, vec!["dir/out"]);
    }

    #[test]
    fn repo_key_carries_no_credentials_and_unifies_remote_spellings() {
        for url in [
            "https://user:token@github.com/org/x.git",
            "https://x-access-token:ghp_secret@GitHub.com/org/x",
            "ssh://git@github.com/org/x",
            "ssh://git@github.com:22/org/x.git",
            "git@github.com:org/x.git",
            "https://github.com/org/x.git?access_token=ghp_secret#frag",
            "https://github.com:443/org/x/",
        ] {
            let key = RepoKey::from_remote_url(url).unwrap();
            assert_eq!(key.as_str(), "github.com/org/x", "{url}");
            for secret in ["token", "ghp_secret", "user", "access_token", "git@"] {
                assert!(!key.as_str().contains(secret), "{url} leaked {secret}");
                assert!(!key.tag_component().contains(secret));
            }
        }
        assert_eq!(
            RepoKey::from_remote_url("https://host:8443/a/b.git")
                .unwrap()
                .as_str(),
            "host:8443/a/b"
        );
    }

    #[test]
    fn local_repositories_key_on_repo_hash() {
        let sandbox = awman_test_support::TestSandbox::new();
        let dir = sandbox.work();
        for origin in [
            None,
            Some("/srv/git/x.git"),
            Some("file:///srv/git/x"),
            Some("../x"),
        ] {
            let key = RepoKey::for_repo(origin, &dir);
            assert_eq!(key.as_str(), repo_hash(&dir), "{origin:?}");
        }
        let remote = RepoKey::for_repo(Some("https://h/o/r"), &dir);
        assert_eq!(remote.as_str(), "h/o/r");
    }

    #[test]
    fn repo_tag_component_is_a_valid_bounded_name_and_distinguishes_lookalikes() {
        let a = RepoKey::from_remote_url("https://h/a/b-c").unwrap();
        let b = RepoKey::from_remote_url("https://h/a/b/c").unwrap();
        assert_ne!(a.tag_component(), b.tag_component());
        let long = RepoKey::from_remote_url(&format!("https://h/{}", "Very_Long".repeat(30)))
            .unwrap()
            .tag_component();
        assert!(long.len() <= REPO_TAG_READABLE_MAX + 1 + REPO_TAG_HASH_LEN);
        for c in [a.tag_component(), long] {
            assert!(c
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "._-".contains(ch)));
            assert!(c.chars().next().unwrap().is_ascii_alphanumeric());
        }
    }
}
