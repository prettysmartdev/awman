//! The release plan: JSON produced by `cargo xtask gates release-plan`
//! (gates-owned schema 1, design 5.4(b) and 7.15). The release module never
//! links the manifest code; it runs its own executable and validates the
//! output here. Required fields have no defaults; only unknown optional
//! fields of a supported schema are ignored.

use super::error::{Step, Stop};
use super::util::{is_sha256, sha256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: u32 = 1;
/// Explicit evidence validity window (design 7.16); the manifest may lower it.
pub const DEFAULT_VALIDITY_DAYS: u32 = 14;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub schema: u32,
    pub manifest_sha256: String,
    pub policy_sha256: String,
    pub gate: String,
    pub ci: CiPlan,
    pub release_workflow: String,
    pub checksums: String,
    pub evidence: String,
    #[serde(default)]
    pub evidence_validity_days: Option<u32>,
    pub assets: Vec<AssetPlan>,
    pub native: Vec<NativePlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CiPlan {
    pub workflow: String,
    pub inputs: BTreeMap<String, String>,
    pub evidence_artifact_prefix: String,
    pub evidence_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetPlan {
    pub asset: String,
    pub build: String,
    pub triple: String,
    pub runs_on: Vec<String>,
    pub toolchain: String,
    pub cargo: Vec<String>,
    pub binary: String,
    pub sign: bool,
    #[serde(default)]
    pub abi: Option<String>,
    pub builder: String,
    pub smoke_suites: Vec<String>,
    #[serde(default)]
    pub native: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NativePlan {
    pub id: String,
    pub asset: String,
    pub host: String,
    pub triple: String,
    pub suites: Vec<String>,
    pub tests: Vec<String>,
    pub fixtures: Vec<FixturePlan>,
    #[serde(default)]
    pub previous_release: Option<serde_json::Value>,
    pub budget_seconds: u64,
    pub required: bool,
    pub kit_artifact: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FixturePlan {
    pub id: String,
    pub file: String,
    pub arch: String,
    /// Absent or null while the pin is pending: `pin-pending`, never a parse error.
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// A pinned input the native check downloads: fixtures and, where the plan
/// names one, the previous release used by the upgrade test.
#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub id: String,
    pub file: String,
    pub sha256: String,
    pub url: String,
}

impl Plan {
    /// Parse and validate. Invalid JSON or a missing required field is
    /// `manifest-stale`; a missing pin is `pin-pending`.
    pub fn parse(bytes: &[u8]) -> Step<(Plan, String)> {
        let plan: Plan = serde_json::from_slice(bytes).map_err(|e| {
            Stop::new(
                "manifest-stale",
                format!("release plan does not match schema {SCHEMA}: {e}"),
            )
        })?;
        plan.validate()?;
        Ok((plan, sha256(bytes)))
    }

    pub fn validate(&self) -> Step {
        let bad = |m: String| Err(Stop::new("manifest-stale", m));
        if self.schema != SCHEMA {
            return bad(format!("unsupported release plan schema {}", self.schema));
        }
        for (name, v) in [
            ("manifest_sha256", &self.manifest_sha256),
            ("policy_sha256", &self.policy_sha256),
        ] {
            if !is_sha256(v) {
                return bad(format!("release plan {name} is not a SHA-256"));
            }
        }
        if self.gate.is_empty() || self.ci.workflow.is_empty() || self.release_workflow.is_empty() {
            return bad("release plan has an empty gate or workflow".into());
        }
        if self.assets.is_empty() {
            return bad("release plan lists no assets".into());
        }
        let mut names = std::collections::BTreeSet::new();
        for a in &self.assets {
            if !names.insert(a.asset.as_str()) {
                return bad(format!("asset {} listed twice", a.asset));
            }
            if a.builder != "hosted" {
                return bad(format!(
                    "asset {} uses builder `{}`; only hosted attested builds can be published",
                    a.asset, a.builder
                ));
            }
            if a.smoke_suites.is_empty() || a.cargo.is_empty() || a.runs_on.is_empty() {
                return bad(format!(
                    "asset {} has no smoke, build or runner selection",
                    a.asset
                ));
            }
            if let Some(n) = &a.native {
                if !self.native.iter().any(|p| &p.id == n) {
                    return bad(format!("asset {} names unknown native check {n}", a.asset));
                }
            }
        }
        for n in &self.native {
            if !n.required {
                return bad(format!(
                    "native check {} is not required; required native checks have no waiver",
                    n.id
                ));
            }
            if !names.contains(n.asset.as_str()) {
                return bad(format!(
                    "native check {} names unknown asset {}",
                    n.id, n.asset
                ));
            }
            if n.tests.is_empty() {
                return bad(format!("native check {} selects no tests", n.id));
            }
            if !matches!(n.host.as_str(), "local" | "ssh") {
                return bad(format!("native check {} has unknown host {}", n.id, n.host));
            }
            let mut seen = std::collections::BTreeSet::new();
            if !n.tests.iter().all(|t| seen.insert(t)) {
                return bad(format!("native check {} lists a test twice", n.id));
            }
        }
        for d in self.downloads_all() {
            if !is_sha256(&d.sha256) || !d.url.starts_with("https://") {
                return Err(Stop::new(
                    "pin-pending",
                    format!(
                        "pinned input {} has no published SHA-256 and https URL",
                        d.id
                    ),
                ));
            }
        }
        Ok(())
    }

    pub fn asset(&self, name: &str) -> Option<&AssetPlan> {
        self.assets.iter().find(|a| a.asset == name)
    }

    pub fn native(&self, id: &str) -> Option<&NativePlan> {
        self.native.iter().find(|n| n.id == id)
    }

    pub fn validity_days(&self) -> u32 {
        self.evidence_validity_days
            .unwrap_or(DEFAULT_VALIDITY_DAYS)
            .min(DEFAULT_VALIDITY_DAYS)
    }

    fn downloads_all(&self) -> Vec<Download> {
        self.native.iter().flat_map(|n| n.downloads()).collect()
    }
}

impl NativePlan {
    pub fn downloads(&self) -> Vec<Download> {
        let mut out: Vec<Download> = self
            .fixtures
            .iter()
            .map(|f| Download {
                id: f.id.clone(),
                file: f.file.clone(),
                sha256: f.sha256.clone().unwrap_or_default(),
                url: f.url.clone().unwrap_or_default(),
            })
            .collect();
        // The upgrade test needs the previous release's exact bytes. Only a
        // resolved `{asset, sha256, url}` object can be downloaded; a bare
        // manifest key is reported as a pending pin, never skipped.
        match &self.previous_release {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(p)) => {
                let field = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                out.push(Download {
                    id: format!("previous-release:{}", self.triple),
                    file: format!("previous-release/{}/{}", self.triple, field("asset")),
                    sha256: field("sha256"),
                    url: field("url"),
                });
            }
            Some(other) => out.push(Download {
                id: format!("previous-release {other} (unresolved)"),
                file: String::new(),
                sha256: String::new(),
                url: String::new(),
            }),
        }
        out
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The checked-in consumer fixture (xtask/tests/release_contract/fixtures).
    pub fn fixture_bytes() -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/release_contract/fixtures/release-plan.json"),
        )
        .unwrap()
    }

    #[test]
    fn the_contract_fixture_parses_with_four_assets_and_two_required_native_checks() {
        let (plan, digest) = Plan::parse(&fixture_bytes()).unwrap();
        assert_eq!(plan.assets.len(), 4);
        assert_eq!(plan.native.len(), 2);
        assert!(is_sha256(&digest));
        assert!(plan.native.iter().all(|n| n.required));
    }

    #[test]
    fn missing_fields_waivers_local_builders_and_pending_pins_are_rejected() {
        let good: serde_json::Value = serde_json::from_slice(&fixture_bytes()).unwrap();
        type Edit = Box<dyn Fn(&mut serde_json::Value)>;
        let cases: Vec<(&str, Edit)> = vec![
            (
                "manifest-stale",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("policy_sha256");
                }),
            ),
            ("manifest-stale", Box::new(|v| v["schema"] = 2.into())),
            (
                "manifest-stale",
                Box::new(|v| v["native"][0]["required"] = false.into()),
            ),
            (
                "manifest-stale",
                Box::new(|v| v["assets"][0]["builder"] = "local".into()),
            ),
            (
                "manifest-stale",
                Box::new(|v| v["native"][1]["tests"] = serde_json::json!([])),
            ),
            (
                "pin-pending",
                Box::new(|v| v["native"][1]["fixtures"][0]["sha256"] = serde_json::Value::Null),
            ),
        ];
        for (code, edit) in cases {
            let mut v = good.clone();
            edit(&mut v);
            let err = Plan::parse(&serde_json::to_vec(&v).unwrap()).unwrap_err();
            assert_eq!(err.code, code, "{err}");
        }
    }
}
