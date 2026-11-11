//! Post-job leak sweep for self-hosted (and hosted) jobs: nothing of this run
//! may outlive it. Leftover processes under the workspace or temp root, test
//! containers and test temp roots are reported, removed best-effort, and make
//! the job fail so the leak is fixed rather than inherited by the next job.

use super::{args::Args, verdict};
use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

pub const TEMP_PREFIX: &str = "awman-test-";
/// Temp roots left by tests: `awman-test-*` directories directly under `parent`.
pub fn leaked_temp_roots(parent: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(parent)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(TEMP_PREFIX))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    let args = Args::parse(args, &[], &["init"])?;
    if args.flag("init") {
        let lease = lease();
        anyhow::ensure!(
            !std::env::var("GITHUB_RUN_ID")
                .unwrap_or_default()
                .is_empty(),
            "job sweep initialization requires a workflow run identity"
        );
        let base = std::env::var_os("RUNNER_TEMP")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("RUNNER_TEMP is required"))?;
        // Unix socket paths inside the run need room for both the run and
        // per-test directories. The marker below still checks the full lease.
        let digest = crate::evidence::digest::input_digest(&lease)?;
        let root = base.join(format!("a-{}", &digest[..12]));
        std::fs::create_dir_all(&root)?;
        let marker = root.join(".awman-owner");
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(mut file) => file.write_all(lease.as_bytes())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                anyhow::ensure!(std::fs::read_to_string(&marker)? == lease, "lease mismatch")
            }
            Err(e) => return Err(e.into()),
        }
        let env = std::env::var_os("GITHUB_ENV")
            .ok_or_else(|| anyhow::anyhow!("GITHUB_ENV is required"))?;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(env)?,
            "AWMAN_TEST_TMPROOT={}",
            root.display()
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let Some(root) = std::env::var_os("AWMAN_TEST_TMPROOT").map(PathBuf::from) else {
        anyhow::bail!(
            "sweep requires the job-owned AWMAN_TEST_TMPROOT; global prefix cleanup is forbidden"
        );
    };
    let root = std::fs::canonicalize(root)?;
    anyhow::ensure!(
        root != std::env::temp_dir() && root.parent().is_some(),
        "sweep refuses a global temp root"
    );
    let owner = root.join(".awman-owner");
    let lease = lease();
    anyhow::ensure!(
        std::fs::read_to_string(&owner).ok().as_deref() == Some(&lease),
        "sweep requires an exact job lease marker"
    );
    let mut leaks: Vec<String> = crate::test::isolation::cleanup_owned_processes(&root)
        .into_iter()
        .map(|pid| format!("process {pid}"))
        .collect();
    // Only this lease's private roots are removable. Process identification by
    // argv substring and ambient Docker name-prefix deletion are forbidden.
    for p in leaked_temp_roots(&root) {
        leaks.push(format!("tempdir: {}", p.display()));
        std::fs::remove_dir_all(&p)?;
    }
    for l in &leaks {
        eprintln!("LEAK: {l}");
    }
    verdict(leaks.is_empty())
}

fn lease() -> String {
    format!(
        "{}:{}:{}:{}",
        std::env::var("GITHUB_RUN_ID").unwrap_or_default(),
        std::env::var("GITHUB_RUN_ATTEMPT").unwrap_or_default(),
        std::env::var("GITHUB_JOB").unwrap_or_default(),
        std::env::var("AWMAN_TEST_LEASE").unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_private_lease_children_are_matched() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("awman-test-abc")).unwrap();
        std::fs::create_dir(d.path().join("unrelated")).unwrap();
        let found = leaked_temp_roots(d.path());
        assert_eq!(found, vec![d.path().join("awman-test-abc")]);
    }
    #[test]
    fn one_lease_cannot_remove_another_leases_diagnostics() {
        let parent = tempfile::tempdir().unwrap();
        let ours = parent.path().join("ours");
        let other = parent.path().join("other");
        for root in [&ours, &other] {
            std::fs::create_dir_all(root.join("awman-test-debug")).unwrap();
        }
        for path in leaked_temp_roots(&ours) {
            std::fs::remove_dir_all(path).unwrap();
        }
        assert!(other.join("awman-test-debug").is_dir());
        assert!(!ours.join("awman-test-debug").exists());
    }
}
