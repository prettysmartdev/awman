//! Cargo discovers roots; syn walks every module irrespective of host cfg.
//! Compiled lists remain mandatory per build; a source scan cannot certify cfg/macro expansion.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};
use syn::{Expr, Item, Lit, Meta};

#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub packages: Vec<Package>,
    pub workspace_members: Vec<String>,
}
#[derive(Debug, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub targets: Vec<Target>,
}
#[derive(Debug, Deserialize)]
pub struct Target {
    pub name: String,
    pub kind: Vec<String>,
    pub src_path: PathBuf,
    #[serde(default, rename = "required-features")]
    pub required_features: Vec<String>,
    /// Cargo's `test = false`: the target is never built as a test harness.
    #[serde(default = "yes")]
    pub test: bool,
}
fn yes() -> bool {
    true
}
#[derive(Debug, Default)]
pub struct Discovery {
    pub reachable: BTreeSet<PathBuf>,
    pub tests: BTreeSet<String>,
    pub functions: BTreeSet<String>,
    pub findings: Vec<String>,
}
pub fn metadata(root: &Path) -> Result<Metadata> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--no-deps", "--locked", "--format-version", "1"])
        .output()?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}
pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    if !root.exists() {
        return Ok(found);
    }
    for item in std::fs::read_dir(root)? {
        let item = item?;
        let path = item.path();
        let ty = item.file_type()?;
        // Never follow directory symlinks, especially private fixture repositories.
        if ty.is_dir() {
            if !["target", ".git", "node_modules"]
                .contains(&item.file_name().to_string_lossy().as_ref())
            {
                found.extend(files(&path)?);
            }
        } else if ty.is_file() && path.extension().is_some_and(|x| x == "rs") {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}
pub fn walk(root: &Path, package: &str, target: &str) -> Result<Discovery> {
    walk_target(root, package, target, false)
}
/// Walk one Cargo target. `test_target` is true for integration-test
/// targets, whose every file is test code.
pub fn walk_target(
    root: &Path,
    package: &str,
    target: &str,
    test_target: bool,
) -> Result<Discovery> {
    let mut d = Discovery::default();
    walk_file(
        root,
        root.parent().context("target root without parent")?,
        &format!("{package}::{target} "),
        &mut d,
        &mut BTreeSet::new(),
        test_target,
    )?;
    Ok(d)
}
fn walk_file(
    path: &Path,
    module_dir: &Path,
    prefix: &str,
    d: &mut Discovery,
    active: &mut BTreeSet<PathBuf>,
    test_code: bool,
) -> Result<()> {
    let path = path
        .canonicalize()
        .with_context(|| format!("missing module {}", path.display()))?;
    ensure!(
        active.insert(path.clone()),
        "module/include cycle at {}",
        path.display()
    );
    d.reachable.insert(path.clone());
    let src = std::fs::read_to_string(&path)?;
    let syntax = syn::parse_file(&src).with_context(|| format!("parse {}", path.display()))?;
    for finding in crate::lint::hygiene::helper_findings(&syntax.items, test_code) {
        d.findings.push(format!("{}: {finding}", path.display()));
    }
    for finding in crate::lint::resources::findings(&syntax.items, test_code, &path) {
        d.findings.push(format!("{}: {finding}", path.display()));
    }
    walk_items(
        &syntax.items,
        &path,
        module_dir,
        path.parent().unwrap(),
        prefix,
        d,
        active,
        test_code,
    )?;
    active.remove(&path);
    Ok(())
}
// The walker's state is spelled out as plain arguments on purpose: each one
// is a different directory or flag of the same recursion.
#[allow(clippy::too_many_arguments)]
fn walk_items(
    items: &[Item],
    file: &Path,
    dir: &Path,
    attribute_dir: &Path,
    prefix: &str,
    d: &mut Discovery,
    active: &mut BTreeSet<PathBuf>,
    test_code: bool,
) -> Result<()> {
    for item in items {
        match item {
            Item::Mod(m) => {
                let name = m.ident.to_string();
                let prefix = format!("{prefix}{name}::");
                let test_code = test_code || crate::lint::hygiene::is_cfg_test(&m.attrs);
                if let Some((_, items)) = &m.content {
                    walk_items(
                        items,
                        file,
                        &dir.join(&name),
                        &dir.join(&name),
                        &prefix,
                        d,
                        active,
                        test_code,
                    )?;
                } else {
                    let paths: Vec<PathBuf> = m
                        .attrs
                        .iter()
                        .filter_map(|a| {
                            if let Meta::NameValue(n) = &a.meta {
                                if n.path.is_ident("path") {
                                    if let Expr::Lit(l) = &n.value {
                                        if let Lit::Str(s) = &l.lit {
                                            return Some(normalize(&attribute_dir.join(s.value())));
                                        }
                                    }
                                }
                            }
                            None
                        })
                        .collect();
                    let paths = if paths.is_empty() {
                        let a = dir.join(format!("{name}.rs"));
                        let b = dir.join(&name).join("mod.rs");
                        if a.exists() {
                            vec![a]
                        } else {
                            vec![b]
                        }
                    } else {
                        paths
                    };
                    for path in paths {
                        let next = if path.file_name().is_some_and(|n| n == "mod.rs") {
                            path.parent().unwrap().to_path_buf()
                        } else {
                            path.with_extension("")
                        };
                        walk_file(&path, &next, &prefix, d, active, test_code)?;
                    }
                }
            }
            Item::Fn(f) => {
                let id = format!("{prefix}{}", f.sig.ident);
                d.functions.insert(id.clone());
                if f.attrs
                    .iter()
                    .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"))
                {
                    d.tests.insert(id.clone());
                    for message in crate::lint::hygiene::test_findings(f) {
                        d.findings
                            .push(format!("{}: {id}: {message}", file.display()));
                    }
                }
            }
            Item::Macro(m) if m.mac.path.is_ident("include") => {
                if let Ok(s) = syn::parse2::<syn::LitStr>(m.mac.tokens.clone()) {
                    let path = file.parent().unwrap().join(s.value());
                    walk_file(&path, dir, prefix, d, active, test_code)?;
                } else {
                    d.findings.push(format!(
                        "{}: unsupported include form needs explicit source metadata",
                        file.display()
                    ));
                }
            }
            Item::Macro(m)
                if m.mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "proptest") =>
            {
                // `proptest! { #[test] fn name(x in strategy) { … } }` expands to one
                // libtest test per `#[test] fn`. Map those names from the macro
                // source; compiled lists (nextest list per build) confirm them.
                let names = proptest_tests(&m.mac.tokens);
                if names.is_empty() {
                    d.findings.push(format!(
                        "{}: proptest! block without a recognisable `#[test] fn` needs explicit discovery metadata",
                        file.display()
                    ));
                }
                for name in names {
                    let id = format!("{prefix}{name}");
                    d.functions.insert(id.clone());
                    d.tests.insert(id);
                }
            }
            _ => {}
        }
    }
    Ok(())
}
/// Names of the `#[test] fn` items written inside a `proptest!` invocation.
/// The token stream prints as `# [test] fn name (…) { … }`, so the names
/// are read from that text: the identifier after the first `fn` that
/// follows a `# [test]` attribute.
fn proptest_tests<T: std::fmt::Display>(tokens: &T) -> Vec<String> {
    let text = tokens.to_string();
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut names = Vec::new();
    let mut saw_test_attr = false;
    for (i, w) in words.iter().enumerate() {
        if *w == "[test]" && i > 0 && words[i - 1] == "#" {
            saw_test_attr = true;
        } else if *w == "fn" && saw_test_attr {
            if let Some(name) = words.get(i + 1) {
                let name: String = name
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    names.push(name);
                }
            }
            saw_test_attr = false;
        }
    }
    names
}
fn target_id(t: &Target) -> String {
    if t.kind.iter().any(|k| k == "lib") {
        "lib".into()
    } else if t.kind.iter().any(|k| k == "test") {
        format!("test:{}", t.name)
    } else {
        format!("bin:{}", t.name)
    }
}
pub fn audit(root: &Path, m: &crate::manifest::Manifest, meta: &Metadata) -> Result<Discovery> {
    let mut all = Discovery::default();
    let mut targets = BTreeSet::new();
    for p in meta
        .packages
        .iter()
        .filter(|p| meta.workspace_members.contains(&p.id))
    {
        for t in &p.targets {
            targets.insert((p.name.clone(), target_id(t)));
            let tests_before = all.tests.len();
            let test_target = t.kind.iter().any(|k| k == "test");
            match walk_target(&t.src_path, &p.name, &t.name, test_target) {
                Ok(d) => {
                    all.reachable.extend(d.reachable);
                    all.tests.extend(d.tests);
                    all.functions.extend(d.functions);
                    all.findings.extend(d.findings.into_iter().filter(|f| {
                        !f.split_once(": ").is_some_and(|(file, message)| {
                            message == "unsupported include form needs explicit source metadata"
                                && m.discovery.allow.iter().any(|row| {
                                    row.reason.starts_with("generated-constants-only:")
                                        && root.join(&row.path) == Path::new(file)
                                })
                        })
                    }));
                }
                Err(e) => all.findings.push(format!(
                    "target {}::{} discovery incomplete: {e:#}",
                    p.name, t.name
                )),
            }
            if !t.test && t.kind.iter().any(|k| k == "bin") {
                // `test = false` binaries (fake CLIs, guest drivers) never run a
                // harness, so a #[test] inside one would silently never run.
                if all.tests.len() > tests_before {
                    all.findings.push(format!(
                        "target {}::{} has test = false but contains tests that would never run",
                        p.name,
                        target_id(t)
                    ));
                }
                continue;
            }
            // A library or executable with no tests is still compiled by its
            // build cell and exercised by component/boundary/artifact suites.
            // Do not invent an empty unit suite merely for a package count.
            if !test_target && all.tests.len() == tests_before {
                continue;
            }
            if !t
                .kind
                .iter()
                .any(|k| ["custom-build", "example"].contains(&k.as_str()))
                && !m.suites.values().any(|s| {
                    s.package.as_deref() == Some(&p.name)
                        && s.target.as_deref() == Some(&target_id(t))
                })
            {
                all.findings.push(format!(
                    "uncovered Cargo target {}::{} (required features {:?})",
                    p.name,
                    target_id(t),
                    t.required_features
                ));
            }
        }
    }
    for (id, s) in &m.suites {
        if s.kind == "nextest"
            && !targets.contains(&(
                s.package.clone().unwrap_or_default(),
                s.target.clone().unwrap_or_default(),
            ))
        {
            all.findings
                .push(format!("suite {id}: missing registered Cargo target"));
        }
    }
    let mut roots = vec![root.join("tests"), root.join("xtask/tests")];
    if root.join("crates").exists() {
        for c in std::fs::read_dir(root.join("crates"))? {
            roots.push(c?.path().join("tests"));
        }
    }
    for r in roots {
        for path in files(&r)? {
            let canonical = path.canonicalize()?;
            if !all.reachable.contains(&canonical) {
                all.findings.push(format!(
                    "unregistered test file {}: not reachable from any Cargo target",
                    path.display()
                ));
            }
        }
    }
    for (id, suite) in &m.suites {
        if suite.kind != "nextest" {
            continue;
        }
        if let Some(prefix) = suite
            .filter
            .as_deref()
            .and_then(|f| f.strip_prefix("test(/^"))
            .and_then(|f| f.strip_suffix("/)"))
            .filter(|f| {
                f.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
            })
        {
            let package = suite.package.as_deref().unwrap_or_default();
            if !all.tests.iter().any(|t| {
                t.starts_with(&format!("{package}::"))
                    && t.split_once(' ')
                        .is_some_and(|(_, name)| name.starts_with(prefix))
            }) {
                all.findings.push(format!(
                    "empty required suite {id}: no source test matches {prefix}"
                ));
            }
        }
    }
    for s in &m.release_critical {
        for test in &s.tests {
            if !all.tests.contains(test) {
                all.findings.push(format!(
                    "release-critical {}: missing executable test {test}",
                    s.id
                ));
            }
        }
    }
    for s in &m.shipped_switch {
        if !all.tests.contains(&s.proof) {
            all.findings.push(format!(
                "shipped switch {}: missing proof test {}",
                s.name, s.proof
            ));
        }
    }
    for q in &m.quarantine {
        if !all.tests.contains(&q.test) {
            all.findings.push(format!(
                "quarantine names missing executable test {}",
                q.test
            ));
        }
    }
    Ok(all)
}
/// Compiled inventory is authoritative. Each identity must have exactly one suite.
pub fn partition(tests: &[String], suites: &BTreeMap<String, Vec<String>>) -> Result<()> {
    ensure!(!tests.is_empty(), "empty required executable inventory");
    let expected: BTreeSet<_> = tests.iter().collect();
    ensure!(
        expected.len() == tests.len(),
        "duplicate compiled test identity"
    );
    for test in &expected {
        let count = suites.values().filter(|v| v.contains(test)).count();
        ensure!(
            count == 1,
            "test {test} belongs to {count} suites; expected exactly one"
        );
    }
    for (id, tests) in suites {
        ensure!(!tests.is_empty(), "empty required suite {id}");
        for t in tests {
            ensure!(expected.contains(t), "suite {id}: unknown test {t}");
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledInventory {
    pub schema: u32,
    pub manifest_version: String,
    pub build: String,
    pub tests: Vec<String>,
    pub suites: BTreeMap<String, Vec<String>>,
    pub source_sha256: BTreeMap<String, String>,
}
pub fn reconcile(
    root: &Path,
    m: &crate::manifest::Manifest,
    build: &str,
    bytes: &[u8],
) -> Result<()> {
    let listed: CompiledInventory = serde_json::from_slice(bytes)?;
    ensure!(
        listed.schema == 1 && listed.manifest_version == m.version() && listed.build == build,
        "stale compiled inventory identity for {build}"
    );
    partition(&listed.tests, &listed.suites)?;
    let required: BTreeSet<_> = m
        .gates
        .keys()
        .map(|g| crate::manifest::validate::cells(m, g))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .filter(|c| c.build == build && m.suites.get(&c.suite).is_some_and(|s| s.kind == "nextest"))
        .map(|c| c.suite)
        .collect();
    let actual: BTreeSet<_> = listed.suites.keys().cloned().collect();
    ensure!(
        actual == required,
        "compiled suite set disagrees with manifest for {build}"
    );
    ensure!(
        !listed.source_sha256.is_empty(),
        "compiled inventory lacks source identity"
    );
    for (path, digest) in listed.source_sha256 {
        ensure!(
            !path.starts_with('/') && !path.split('/').any(|s| s == ".."),
            "unsafe source path in inventory"
        );
        ensure!(
            crate::evidence::digest::sha256_file(&root.join(&path))? == digest,
            "compiled inventory source changed: {path}"
        );
    }
    Ok(())
}
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
