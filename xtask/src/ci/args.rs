//! Tiny flag parser for the helper commands: `--name value` pairs plus
//! positional arguments. Unknown flags are errors so typos fail loudly.

use anyhow::{bail, Result};
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct Args {
    pub flags: BTreeMap<String, Vec<String>>,
    pub positional: Vec<String>,
}

impl Args {
    /// `bools` are flags without a value.
    pub fn parse(args: &[String], known: &[&str], bools: &[&str]) -> Result<Args> {
        let mut out = Args::default();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                if bools.contains(&name) {
                    out.flags
                        .entry(name.into())
                        .or_default()
                        .push("true".into());
                } else if known.contains(&name) {
                    i += 1;
                    let Some(v) = args.get(i) else {
                        bail!("--{name} needs a value");
                    };
                    out.flags.entry(name.into()).or_default().push(v.clone());
                } else {
                    bail!("unknown flag --{name}");
                }
            } else {
                out.positional.push(a.clone());
            }
            i += 1;
        }
        Ok(out)
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }
    pub fn need(&self, name: &str) -> Result<&str> {
        self.get(name)
            .ok_or_else(|| anyhow::anyhow!("--{name} is required"))
    }
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.flags
            .get(name)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn parses_flags_positionals_and_repeats() {
        let a = Args::parse(
            &s(&[
                "--out", "f", "a=b", "--expect", "x=1", "--expect", "y=2", "--q",
            ]),
            &["out", "expect"],
            &["q"],
        )
        .unwrap();
        assert_eq!(a.need("out").unwrap(), "f");
        assert_eq!(a.all("expect"), vec!["x=1", "y=2"]);
        assert_eq!(a.positional, vec!["a=b"]);
        assert!(a.flag("q"));
    }
    #[test]
    fn unknown_flag_and_missing_value_fail() {
        assert!(Args::parse(&s(&["--nope"]), &["out"], &[]).is_err());
        assert!(Args::parse(&s(&["--out"]), &["out"], &[]).is_err());
    }
}
