//! Small helpers shared by the pipeline subcommands: flag parsing, GitHub step
//! outputs and the `blocked`/`failed` exit convention.
//!
//! Exit codes follow xtask (0 ok, 1 check failed, 2 could not run). A
//! `blocked` result (infrastructure: missing runner, store, credential, pin)
//! and a `failed` result (product or evidence) both exit 1 and print one line
//! that starts with `BLOCKED:` or `FAILED:` so the job log and the summary say
//! which kind of red it is. Nothing is published on either path.

use anyhow::{bail, Context, Result};
use std::{collections::BTreeMap, io::Write, path::PathBuf, process::ExitCode};

/// `--name value` and `--flag` arguments; positional arguments are refused.
#[derive(Debug, Default)]
pub struct Flags {
    values: BTreeMap<String, Vec<String>>,
}

impl Flags {
    /// Parse `args`. `switches` lists the flags that take no value.
    pub fn parse(args: &[String], switches: &[&str]) -> Result<Flags> {
        let mut flags = Flags::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let Some(name) = arg.strip_prefix("--") else {
                bail!("unexpected argument `{arg}`");
            };
            let (name, value) = match name.split_once('=') {
                Some((n, v)) => (n.to_owned(), v.to_owned()),
                None if switches.contains(&name) => (name.to_owned(), "true".to_owned()),
                None => {
                    let v = it
                        .next()
                        .with_context(|| format!("--{name} needs a value"))?;
                    (name.to_owned(), v.clone())
                }
            };
            flags.values.entry(name).or_default().push(value);
        }
        Ok(flags)
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }
    pub fn req(&self, name: &str) -> Result<&str> {
        self.get(name)
            .with_context(|| format!("--{name} is required"))
    }
    pub fn all(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }
    pub fn on(&self, name: &str) -> bool {
        matches!(self.get(name), Some("true" | "1" | "yes"))
    }
    pub fn path(&self, name: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(self.req(name)?))
    }
}

/// Append `key=value` lines to `$GITHUB_OUTPUT` when set, and always echo
/// them to stdout so a local run shows the same decision.
pub fn outputs(pairs: &[(&str, String)]) -> Result<()> {
    let mut text = String::new();
    for (k, v) in pairs {
        if v.contains('\n') {
            text.push_str(&format!("{k}<<AWMAN_EOF\n{v}\nAWMAN_EOF\n"));
        } else {
            text.push_str(&format!("{k}={v}\n"));
        }
    }
    print!("{text}");
    if let Some(path) = std::env::var_os("GITHUB_OUTPUT") {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .context("opening GITHUB_OUTPUT")?;
        f.write_all(text.as_bytes())?;
    }
    Ok(())
}

/// Append markdown to the job summary when running in Actions.
pub fn summary(markdown: &str) {
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
        {
            let _ = f.write_all(markdown.as_bytes());
        }
    }
}

/// Why a step stopped. Both kinds exit 1; the prefix tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Infrastructure: capacity, credentials, service, pins. Rerun when fixed.
    Blocked(String),
    /// Product or evidence: needs a new commit or a new candidate.
    Failed(String),
}

impl Stop {
    pub fn report(&self) -> ExitCode {
        let line = self.to_string();
        eprintln!("{line}");
        summary(&format!("\n**{line}**\n"));
        ExitCode::from(1)
    }
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stop::Blocked(r) => write!(f, "BLOCKED: {r}"),
            Stop::Failed(r) => write!(f, "FAILED: {r}"),
        }
    }
}

/// This job's identity in Actions (`GITHUB_RUN_ID`, `GITHUB_RUN_ATTEMPT`,
/// `GITHUB_JOB`). Zero outside Actions: such a run can never satisfy a gate.
pub fn run_env() -> super::record::Producer {
    let n = |k: &str| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    super::record::Producer {
        run_id: n("GITHUB_RUN_ID"),
        attempt: n("GITHUB_RUN_ATTEMPT") as u32,
        job: std::env::var("GITHUB_JOB").unwrap_or_default(),
    }
}

pub fn write_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flags_parse_values_switches_and_equals() {
        let f = Flags::parse(
            &s(&["--version", "v1.2.3", "--dry-run", "--out=x"]),
            &["dry-run"],
        )
        .unwrap();
        assert_eq!(f.get("version"), Some("v1.2.3"));
        assert!(f.on("dry-run"));
        assert_eq!(f.get("out"), Some("x"));
        assert!(Flags::parse(&s(&["positional"]), &[]).is_err());
        assert!(Flags::parse(&s(&["--version"]), &[]).is_err());
    }

    #[test]
    fn stop_lines_name_their_kind() {
        assert_eq!(Stop::Blocked("x".into()).to_string(), "BLOCKED: x");
        assert_eq!(Stop::Failed("y".into()).to_string(), "FAILED: y");
    }
}
