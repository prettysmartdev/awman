//! Conformance checks that run unchanged against a fake (tier `component`)
//! and the real thing (tier `boundary`). A fake that passes here and a real
//! tool that passes here agree on everything awman relies on.

use crate::fixtures::OciImageBuilder;
use crate::http;
use crate::TestSandbox;
use std::net::SocketAddr;
use std::path::PathBuf;

/// The result of one conformance run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConformanceReport {
    /// What was checked (e.g. `docker CLI /tmp/x/bin/docker`).
    pub subject: String,
    /// Names of passed checks.
    pub passed: Vec<String>,
    /// Failed checks with the reason.
    pub failed: Vec<(String, String)>,
}

impl ConformanceReport {
    fn new(subject: String) -> ConformanceReport {
        ConformanceReport {
            subject,
            ..ConformanceReport::default()
        }
    }

    fn check(&mut self, name: &str, result: Result<(), String>) {
        match result {
            Ok(()) => self.passed.push(name.to_string()),
            Err(reason) => self.failed.push((name.to_string(), reason)),
        }
    }

    /// Whether every check passed.
    pub fn is_success(&self) -> bool {
        self.failed.is_empty() && !self.passed.is_empty()
    }

    /// Panic listing every failed check.
    pub fn assert_conforms(&self) {
        assert!(
            self.is_success(),
            "{} does not conform:\n{}",
            self.subject,
            self.failed
                .iter()
                .map(|(n, r)| format!("  {n}: {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// A docker CLI to check: run through `sandbox` (with its environment, e.g.
/// after `sandbox.grant(&docker)` for the real one).
pub struct DockerCliUnderTest<'a> {
    /// The sandbox whose supervisor runs the CLI.
    pub sandbox: &'a TestSandbox,
    /// The CLI executable.
    pub cli: PathBuf,
}

/// Docker CLI contract awman relies on: version and server version queries
/// succeed with a version string; inspecting a missing image fails with a
/// non-zero exit and "No such image".
pub fn docker_cli(cli: &DockerCliUnderTest) -> ConformanceReport {
    let mut report = ConformanceReport::new(format!("docker CLI {}", cli.cli.display()));
    let run = |args: &[&str]| {
        cli.sandbox
            .command(&cli.cli)
            .args(args)
            .label("docker (conformance)")
            .run()
    };
    let looks_like_version = |text: &str| {
        let t = text.trim();
        !t.is_empty() && t.chars().next().is_some_and(|c| c.is_ascii_digit())
    };
    let out = run(&["version", "--format", "{{.Client.Version}}"]);
    report.check(
        "client_version",
        if out.exit.success() && looks_like_version(&out.stdout_text()) {
            Ok(())
        } else {
            Err(format!("{} stdout={:?}", out.exit, out.stdout_text()))
        },
    );
    let out = run(&["info", "--format", "{{.ServerVersion}}"]);
    report.check(
        "server_version",
        if out.exit.success() && looks_like_version(&out.stdout_text()) {
            Ok(())
        } else {
            Err(format!(
                "{} stdout={:?} stderr={:?}",
                out.exit,
                out.stdout_text(),
                out.stderr_text()
            ))
        },
    );
    let out = run(&["image", "inspect", "awman-conformance-missing:none"]);
    report.check(
        "missing_image_fails",
        if !out.exit.success() && out.stderr_text().to_lowercase().contains("no such image") {
            Ok(())
        } else {
            Err(format!("{} stderr={:?}", out.exit, out.stderr_text()))
        },
    );
    report
}

/// A registry to check.
pub struct RegistryUnderTest {
    /// `host:port` on loopback.
    pub addr: SocketAddr,
    /// Repository to push into (e.g. `awman-conformance/image`).
    pub repository: String,
}

/// OCI distribution contract awman relies on: API root, monolithic and
/// chunked blob upload with digest verification, manifest push/pull by tag
/// and digest, tag listing, and 404 for unknown manifests.
pub fn registry(reg: &RegistryUnderTest) -> ConformanceReport {
    let mut report = ConformanceReport::new(format!("registry {}", reg.addr));
    let name = &reg.repository;
    let image = OciImageBuilder::new()
        .layer(&[("conformance", b"awman registry conformance\n")])
        .build();
    let req = |method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]| {
        http::request(reg.addr, method, target, headers, body).map_err(|e| e.to_string())
    };
    report.check(
        "api_root",
        req("GET", "/v2/", &[], b"").and_then(|r| expect(r.status, 200)),
    );

    let blob = &image.layers[0];
    let monolithic = req(
        "POST",
        &format!("/v2/{name}/blobs/uploads/?digest={}", blob.digest),
        &[("Content-Type", "application/octet-stream")],
        &blob.bytes,
    )
    .and_then(|r| expect(r.status, 201));
    report.check("blob_monolithic_upload", monolithic);

    let config = &image.config;
    let chunked = req("POST", &format!("/v2/{name}/blobs/uploads/"), &[], b"").and_then(|r| {
        expect(r.status, 202)?;
        let location = r.get_header("location").ok_or("no Location")?.to_string();
        let path = location
            .split_once("://")
            .map(|(_, rest)| {
                rest.split_once('/')
                    .map(|(_, p)| format!("/{p}"))
                    .unwrap_or_default()
            })
            .unwrap_or(location);
        let sep = if path.contains('?') { '&' } else { '?' };
        let r = req(
            "PUT",
            &format!("{path}{sep}digest={}", config.digest),
            &[("Content-Type", "application/octet-stream")],
            &config.bytes,
        )?;
        expect(r.status, 201)
    });
    report.check("blob_upload_session", chunked);

    let wrong = req(
        "POST",
        &format!("/v2/{name}/blobs/uploads/?digest=sha256:{}", "0".repeat(64)),
        &[],
        b"not matching",
    )
    .and_then(|r| {
        if r.status == 400 {
            Ok(())
        } else {
            Err(format!("status {}", r.status))
        }
    });
    report.check("blob_digest_verified", wrong);

    let head = req(
        "HEAD",
        &format!("/v2/{name}/blobs/{}", blob.digest),
        &[],
        b"",
    )
    .and_then(|r| {
        expect(r.status, 200)?;
        match r.get_header("docker-content-digest") {
            Some(d) if d == blob.digest => Ok(()),
            other => Err(format!("Docker-Content-Digest {other:?}")),
        }
    });
    report.check("blob_head", head);

    let get = req(
        "GET",
        &format!("/v2/{name}/blobs/{}", blob.digest),
        &[],
        b"",
    )
    .and_then(|r| {
        expect(r.status, 200)?;
        if r.body == blob.bytes {
            Ok(())
        } else {
            Err("blob bytes differ".into())
        }
    });
    report.check("blob_get", get);

    let put = req(
        "PUT",
        &format!("/v2/{name}/manifests/conformance"),
        &[("Content-Type", &image.manifest.media_type)],
        &image.manifest.bytes,
    )
    .and_then(|r| expect(r.status, 201));
    report.check("manifest_put", put);

    let accept = [("Accept", image.manifest.media_type.as_str())];
    let by_tag = req(
        "GET",
        &format!("/v2/{name}/manifests/conformance"),
        &accept,
        b"",
    )
    .and_then(|r| {
        expect(r.status, 200)?;
        if r.body == image.manifest.bytes {
            Ok(())
        } else {
            Err("manifest bytes differ".into())
        }
    });
    report.check("manifest_get_by_tag", by_tag);

    let by_digest = req(
        "GET",
        &format!("/v2/{name}/manifests/{}", image.manifest.digest),
        &accept,
        b"",
    )
    .and_then(|r| expect(r.status, 200));
    report.check("manifest_get_by_digest", by_digest);

    let tags = req("GET", &format!("/v2/{name}/tags/list"), &[], b"").and_then(|r| {
        expect(r.status, 200)?;
        let v: serde_json::Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
        let has = v["tags"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t == "conformance"));
        if has {
            Ok(())
        } else {
            Err(format!("tags {v}"))
        }
    });
    report.check("tags_list", tags);

    let missing = req(
        "GET",
        &format!("/v2/{name}/manifests/does-not-exist"),
        &accept,
        b"",
    )
    .and_then(|r| expect(r.status, 404));
    report.check("manifest_unknown_is_404", missing);
    report
}

fn expect(actual: u16, wanted: u16) -> Result<(), String> {
    if actual == wanted {
        Ok(())
    } else {
        Err(format!("status {actual}, expected {wanted}"))
    }
}

/// A work server to check.
pub struct WorkServerUnderTest<'a> {
    /// Its base URL.
    pub base_url: String,
    /// Out-of-band administration (the mock implements it directly).
    pub admin: &'a dyn awman_wsp::conformance::ConformanceAdmin,
}

/// The Work Server Protocol v1 conformance suite of `awman-wsp`.
pub async fn work_server(server: &WorkServerUnderTest<'_>) -> ConformanceReport {
    let report = awman_wsp::conformance::run(&server.base_url, server.admin).await;
    ConformanceReport {
        subject: format!("work server {}", server.base_url),
        passed: report.passed.iter().map(|s| s.to_string()).collect(),
        failed: report
            .failed
            .iter()
            .map(|(n, r)| (n.to_string(), r.clone()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_report_does_not_conform() {
        let report = ConformanceReport::new("x".into());
        assert!(!report.is_success());
        let mut ok = ConformanceReport::new("y".into());
        ok.check("a", Ok(()));
        assert!(ok.is_success());
        ok.check("b", Err("bad".into()));
        let err = std::panic::catch_unwind(|| ok.assert_conforms()).unwrap_err();
        assert!(crate::panic_text(&err).contains("b: bad"));
    }
}
