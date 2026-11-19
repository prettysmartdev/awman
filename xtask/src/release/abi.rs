//! Distribution ABI policy. Native developer packets record their host ABI;
//! only the published GNU x86_64 asset must fit the supported glibc baseline.
use anyhow::{ensure, Context, Result};
use std::{collections::BTreeSet, fs, path::Path, process::Command};

const GLIBC_CEILING: &[u32] = &[2, 35];

fn glibc_versions(text: &str) -> Result<BTreeSet<Vec<u32>>> {
    let mut versions = BTreeSet::new();
    for word in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.') {
        let Some(version) = word.strip_prefix("GLIBC_") else {
            continue;
        };
        let mut parts = version
            .split('.')
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("unsupported glibc symbol version {word}"))?;
        ensure!(
            (2..=3).contains(&parts.len()),
            "malformed glibc symbol version {word}"
        );
        while parts.len() > 2 && parts.last() == Some(&0) {
            parts.pop();
        }
        versions.insert(parts);
    }
    ensure!(
        !versions.is_empty(),
        "GNU release has no inspectable glibc symbol requirements"
    );
    ensure!(versions.iter().all(|version| version.as_slice() <= GLIBC_CEILING),
        "Linux x86_64 release requires glibc newer than 2.35: {versions:?}; build on the declared Ubuntu 22.04 release runner");
    Ok(versions)
}

pub(super) fn verify(target: &str, executable: &Path, out: &Path) -> Result<()> {
    if target != "x86_64-unknown-linux-gnu" {
        return Ok(());
    }
    let scan = Command::new("readelf")
        .args(["--version-info", "--wide"])
        .arg(executable)
        .output()
        .context("release ABI scan requires binutils readelf")?;
    ensure!(
        scan.status.success(),
        "readelf could not inspect the Linux release ABI"
    );
    let text = String::from_utf8(scan.stdout)?;
    let versions = glibc_versions(&text)?;
    fs::write(
        out.join("abi.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "artifact_sha256": crate::evidence::digest::sha256_file(executable)?,
            "glibc_ceiling": "2.35", "required_versions": versions, "readelf": text
        }))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_abi_accepts_the_baseline_and_rejects_new_or_unreadable_requirements() {
        assert!(
            glibc_versions("Name: GLIBC_2.2.5 Flags: none\nName: GLIBC_2.35\n(2) GLIBC_2.34")
                .is_ok()
        );
        for text in [
            "Name: GLIBC_2.36",
            "Name: GLIBC_2.41",
            "Name: GLIBC_ABI_DT_RELR",
            "Name: GLIBC_PRIVATE",
            "Name: GLIBC_2",
            "not ELF output",
        ] {
            assert!(glibc_versions(text).is_err(), "{text}");
        }
    }
    #[test]
    fn linux_release_uses_the_abi_baseline_runner_and_keeps_the_static_smoke() {
        let m = crate::manifest::load(&crate::gates::root()).unwrap();
        let release = &m.builds["linux-x64-release"];
        assert_eq!(m.platforms[&release.platform].runs_on, ["ubuntu-22.04"]);
        assert_eq!(release.features, "builtin");
        assert_eq!(
            m.builds["linux-x64-musl-static"].target.as_deref(),
            Some("x86_64-unknown-linux-musl")
        );
    }
}
