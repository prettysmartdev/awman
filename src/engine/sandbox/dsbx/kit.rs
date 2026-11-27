//! Deterministic, atomic v3 kit emission. No credential values or launch state.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::data::templates::{
    sbx_apply_script, sbx_dockerfile_for, sbx_kit_template_for, sbx_launch_script,
};
use crate::engine::agent::sbx_contract::{
    kit_digest, EmittedKit, KitKind, KitManifest, SbxRoute, APPLY_PROTOCOL, KIT_ID_REL,
    KIT_MANIFEST_FILE, KIT_MANIFEST_SCHEMA,
};
use crate::engine::error::EngineError;

pub(super) struct DSbxKitEmitter;

impl Default for DSbxKitEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl DSbxKitEmitter {
    pub(super) fn new() -> Self {
        Self
    }

    /// Emit a complete v3 build context. Concurrent emitters serialize on a
    /// sibling lock directory; atomic replacements leave current files alone.
    /// The legacy `<kits>/<agent>` tree is never read, written or removed.
    pub(super) fn emit_for_agent(&self, agent: &str, dest: &Path) -> Result<(), EngineError> {
        let route = crate::engine::agent::sbx_contract::sbx_route(agent)
            .filter(SbxRoute::is_supported)
            .ok_or_else(|| {
                EngineError::Sandbox(format!(
                    "agent '{agent}' has no Docker Sandboxes kit; select a container runtime"
                ))
            })?;
        let handle = route.kit_handle().expect("supported route has handle");
        if dest.file_name().and_then(|s| s.to_str()) != Some(&handle)
            || dest
                .parent()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                != Some("v3")
        {
            return Err(EngineError::Sandbox(format!(
                "v3 kit destination must be <kits>/v3/{handle}"
            )));
        }
        let parent = dest.parent().expect("v3 kit has parent");
        ensure_directory(parent)?;
        let lock = parent.join(format!(".{handle}.emit-lock"));
        let _lock = EmitLock::acquire(&lock)?;
        ensure_directory(dest)?;
        let manifest = KitManifest {
            schema: KIT_MANIFEST_SCHEMA,
            agent: agent.into(),
            kind: if route.is_owned_workload() {
                KitKind::Workload
            } else {
                KitKind::Mixin
            },
            handle: handle.clone(),
            descriptor: format!("{handle}.yaml"),
            dockerfile: format!("{handle}.dockerfile"),
            apply_protocol: APPLY_PROTOCOL,
        };
        let allowlist = route
            .managed_files
            .iter()
            .map(|file| {
                format!(
                    "{}|{}|{}",
                    file.id,
                    file.path,
                    if file.root_owned { "root" } else { "agent" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let apply = sbx_apply_script()
            .replace("{{AWMAN_AGENT}}", agent)
            .replace("{{AWMAN_MANAGED_FILES}}", &allowlist);
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| EngineError::Sandbox(e.to_string()))?;
        let mut files: BTreeMap<String, (Vec<u8>, u32)> = BTreeMap::from([
            (
                manifest.descriptor.clone(),
                (
                    sbx_kit_template_for(agent)
                        .expect("matrix descriptor")
                        .as_bytes()
                        .to_vec(),
                    0o644,
                ),
            ),
            (
                manifest.dockerfile.clone(),
                (
                    sbx_dockerfile_for(agent)
                        .expect("matrix recipe")
                        .as_bytes()
                        .to_vec(),
                    0o644,
                ),
            ),
            (KIT_MANIFEST_FILE.into(), (manifest_bytes, 0o644)),
            (
                "rootfs/usr/local/lib/awman/awman-apply".into(),
                (apply.into_bytes(), 0o755),
            ),
        ]);
        if route.is_owned_workload() {
            files.insert(
                "rootfs/usr/local/lib/awman/awman-launch".into(),
                (sbx_launch_script().as_bytes().to_vec(), 0o755),
            );
        }
        prune(dest, dest, &files)?;
        for (relative, (bytes, mode)) in &files {
            atomic_write(&dest.join(relative), bytes, *mode, &lock)?;
        }
        let digest = kit_digest(dest).map_err(|e| EngineError::Sandbox(e.to_string()))?;
        atomic_write(
            &dest.join(KIT_ID_REL),
            format!("{digest}\n").as_bytes(),
            0o644,
            &lock,
        )?;
        EmittedKit::load(dest).map_err(|e| EngineError::Sandbox(e.to_string()))?;
        Ok(())
    }
}

struct EmitLock<'a>(&'a Path);

impl<'a> EmitLock<'a> {
    fn acquire(path: &'a Path) -> Result<Self, EngineError> {
        let start = Instant::now();
        loop {
            match std::fs::create_dir(path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() >= Duration::from_secs(10) {
                        return Err(EngineError::Sandbox(format!("kit emission lock {} is busy; if no awman emitter is running, remove this lock directory and retry", path.display())));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(EngineError::io(path, e)),
            }
        }
    }
}

impl Drop for EmitLock<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}

fn ensure_directory(path: &Path) -> Result<(), EngineError> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(EngineError::Sandbox(format!(
            "kit path {} is not a directory",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                ensure_directory(parent)?;
            }
            std::fs::create_dir(path).map_err(|e| EngineError::io(path, e))
        }
        Err(e) => Err(EngineError::io(path, e)),
    }
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32, lock: &Path) -> Result<(), EngineError> {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return set_mode(path, mode);
    }
    ensure_directory(path.parent().expect("kit file has parent"))?;
    let tmp = lock.join("pending");
    std::fs::write(&tmp, bytes).map_err(|e| EngineError::io(&tmp, e))?;
    set_mode(&tmp, mode)?;
    std::fs::rename(&tmp, path).map_err(|e| EngineError::io(path, e))
}

fn prune(
    root: &Path,
    dir: &Path,
    expected: &BTreeMap<String, (Vec<u8>, u32)>,
) -> Result<(), EngineError> {
    for entry in std::fs::read_dir(dir).map_err(|e| EngineError::io(dir, e))? {
        let entry = entry.map_err(|e| EngineError::io(dir, e))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| EngineError::io(&path, e))?;
        if meta.is_dir() {
            prune(root, &path, expected)?;
            if std::fs::read_dir(&path)
                .map_err(|e| EngineError::io(&path, e))?
                .next()
                .is_none()
            {
                std::fs::remove_dir(&path).map_err(|e| EngineError::io(&path, e))?;
            }
        } else if meta.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("descendant")
                .to_string_lossy()
                .replace('\\', "/");
            if relative != KIT_ID_REL && !expected.contains_key(&relative) {
                std::fs::remove_file(&path).map_err(|e| EngineError::io(&path, e))?;
            }
        } else {
            return Err(EngineError::Sandbox(format!(
                "kit entry {} is not a regular file or directory",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| EngineError::io(path, e))?;
    if meta.permissions().mode() & 0o777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| EngineError::io(path, e))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<(), EngineError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent::sbx_contract::{kit_dir_v3, sbx_route, SBX_AGENTS};
    use awman_test_support::TestSandbox;

    #[test]
    fn emits_each_admitted_v3_build_context_and_refuses_the_other_two() {
        let sb = TestSandbox::named("engine::sandbox::dsbx::kit::tests::emits_each_admitted_v3_build_context_and_refuses_the_other_two");
        let root = sb.awman_home().join("kits");
        for agent in SBX_AGENTS {
            let route = sbx_route(agent).unwrap();
            if !route.is_supported() {
                let dest = root.join("v3").join(format!("awman-{agent}"));
                assert!(DSbxKitEmitter::new().emit_for_agent(agent, &dest).is_err());
                assert!(!dest.exists());
                continue;
            }
            let dest = kit_dir_v3(&root, agent).unwrap();
            DSbxKitEmitter::new().emit_for_agent(agent, &dest).unwrap();
            let kit = EmittedKit::load(&dest).unwrap();
            assert_eq!(kit.manifest.agent, agent);
            let script_path = dest.join("rootfs/usr/local/lib/awman/awman-apply");
            let script = std::fs::read_to_string(&script_path).unwrap();
            assert!(!script.contains("{{"));
            assert!(script.contains(&format!("agent {agent}")));
            for file in route.managed_files {
                assert!(script.contains(&format!(
                    "{}|{}|{}",
                    file.id,
                    file.path,
                    if file.root_owned { "root" } else { "agent" }
                )));
            }
            assert_eq!(
                dest.join("rootfs/usr/local/lib/awman/awman-launch")
                    .exists(),
                route.is_owned_workload()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(script_path).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }
            assert!(!root.join(agent).exists());
            assert!(!dest.join("spec.yaml").exists());
        }
        assert!(DSbxKitEmitter::new()
            .emit_for_agent("unknown", &root.join("v3/unknown"))
            .is_err());
        sb.finish();
    }

    #[test]
    fn reemission_is_stable_prunes_obsolete_files_and_preserves_legacy_kits() {
        let sb = TestSandbox::named("engine::sandbox::dsbx::kit::tests::reemission_is_stable_prunes_obsolete_files_and_preserves_legacy_kits");
        let root = sb.awman_home().join("kits");
        let legacy = root.join("claude/spec.yaml");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "legacy").unwrap();
        let dest = kit_dir_v3(&root, "claude").unwrap();
        let emitter = DSbxKitEmitter::new();
        emitter.emit_for_agent("claude", &dest).unwrap();
        let first = EmittedKit::load(&dest).unwrap();
        let mtime = std::fs::metadata(dest.join(KIT_ID_REL))
            .unwrap()
            .modified()
            .unwrap();
        emitter.emit_for_agent("claude", &dest).unwrap();
        assert_eq!(
            mtime,
            std::fs::metadata(dest.join(KIT_ID_REL))
                .unwrap()
                .modified()
                .unwrap()
        );
        std::fs::write(dest.join("obsolete"), "old").unwrap();
        assert!(EmittedKit::load(&dest).is_err());
        emitter.emit_for_agent("claude", &dest).unwrap();
        assert!(!dest.join("obsolete").exists());
        assert_eq!(first.digest, EmittedKit::load(&dest).unwrap().digest);
        assert_eq!(std::fs::read_to_string(legacy).unwrap(), "legacy");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    DSbxKitEmitter::new()
                        .emit_for_agent("claude", &dest)
                        .unwrap()
                });
            }
        });
        assert_eq!(first, EmittedKit::load(&dest).unwrap());
        sb.finish();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_in_the_build_context() {
        let sb = TestSandbox::named(
            "engine::sandbox::dsbx::kit::tests::rejects_symlinks_in_the_build_context",
        );
        let dest = kit_dir_v3(&sb.awman_home().join("kits"), "codex").unwrap();
        DSbxKitEmitter::new()
            .emit_for_agent("codex", &dest)
            .unwrap();
        let outside = sb.work().join("keep");
        std::fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("link")).unwrap();
        assert!(DSbxKitEmitter::new()
            .emit_for_agent("codex", &dest)
            .is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "unchanged");
        sb.finish();
    }
}

// Shell interpretation needs a process, so these inline tests belong to the
// component tier. They execute only bundled policy helpers, never host agents.
#[cfg(all(test, target_os = "linux"))]
mod script_tests {
    use super::*;
    use crate::engine::agent::sbx_contract::{
        kit_dir_v3, sha256_hex, ApplyDocument, ApplyFile, CLAUDE_MEMORY,
    };
    use awman_test_support::{Exit, Stdin, TestCase, TestSandbox};

    fn sandbox(id: &'static str) -> TestSandbox {
        let mut sb = TestSandbox::for_test(TestCase::component(id));
        sb.allow_tool(Path::new("/bin/bash"));
        sb
    }

    #[test]
    fn every_missing_helper_is_a_failure_and_an_unverified_gate_never_launches() {
        let sb = sandbox("engine::sandbox::dsbx::kit::script_tests::every_missing_helper_is_a_failure_and_an_unverified_gate_never_launches");
        let source = sbx_apply_script();
        let helpers = source
            .split("for helper in ")
            .nth(1)
            .unwrap()
            .split("; do")
            .next()
            .unwrap();
        let check = source
            .split("for helper in ")
            .nth(1)
            .unwrap()
            .split("\ndone")
            .next()
            .unwrap();
        let fail = source
            .lines()
            .find(|line| line.starts_with("fail()"))
            .unwrap();
        // Execute the actual helper admission loop independently of uid. This
        // makes the test useful on unprivileged hosts without faking root.
        let probe = format!("{fail}\nfor helper in {check}\ndone\n");
        let probe_path = sb.tmp().join("helper-probe.sh");
        std::fs::write(&probe_path, probe).unwrap();
        for missing in helpers.split_whitespace() {
            let dir = sb.tmp().join(missing);
            std::fs::create_dir(&dir).unwrap();
            for helper in helpers.split_whitespace().filter(|h| *h != missing) {
                std::os::unix::fs::symlink("/bin/true", dir.join(helper)).unwrap();
            }
            let out = sb
                .command("/bin/bash")
                .arg(&probe_path)
                .env("PATH", &dir)
                .run();
            assert_eq!(out.exit, Exit::Code(69), "{missing}: {}", out.stderr_text());
            assert!(out
                .stderr_text()
                .contains(&format!("required helper {missing} missing")));
        }
        let uid_gate = source
            .lines()
            .find(|line| line.starts_with("[[ $(id -u agent)"))
            .unwrap();
        let wrong_uid = format!("{fail}\nid() {{ printf '1001\\n'; }}\n{uid_gate}\n");
        let out = sb
            .command("/bin/bash")
            .stdin(Stdin::Bytes(wrong_uid.into_bytes()))
            .run();
        assert_eq!(out.exit, Exit::Code(69));
        assert!(out.stderr_text().contains("agent must have uid 1000"));
        assert!(!source.contains("jq"));
        for script in [source, sbx_launch_script()] {
            sb.command("/bin/bash")
                .arg("-n")
                .stdin(Stdin::Bytes(script.as_bytes().to_vec()))
                .run()
                .assert_success();
        }
        let gate_path = sb.tmp().join("launch-gate.sh");
        std::fs::write(&gate_path, sbx_launch_script()).unwrap();
        let out = sb
            .command("/bin/bash")
            .arg(&gate_path)
            .env_remove("AWMAN_LAUNCH_ID")
            .run();
        assert_eq!(out.exit, Exit::Code(78));
        assert!(out.stderr_text().contains("no verified awman policy"));
        sb.finish();
    }

    fn private_apply_script(sb: &TestSandbox) -> (EmittedKit, std::path::PathBuf) {
        let dest = kit_dir_v3(&sb.awman_home().join("kits"), "claude").unwrap();
        DSbxKitEmitter::new()
            .emit_for_agent("claude", &dest)
            .unwrap();
        let kit = EmittedKit::load(&dest).unwrap();
        let source =
            std::fs::read_to_string(dest.join("rootfs/usr/local/lib/awman/awman-apply")).unwrap();
        // Test-only transport maps every guest write into private state and
        // uses our uid for both ownership branches. Actual uid separation
        // remains a live Linux-guest boundary obligation.
        let mut script = source
            .replace("[[ $EUID == 0 ]] || fail 77 'must run as root'", ":")
            .replace(
                "[[ $(id -u agent) == 1000 ]] || fail 69 'agent must have uid 1000'",
                ":",
            );
        script = script
            .replace("runuser -u agent -- /bin/bash", "/bin/bash")
            .replace("-o root -g root ", "");
        for guest in [
            "/usr/local/lib/awman",
            "/var/lib/awman",
            "/etc/claude-code",
            "/home/agent",
            "/tmp/awman-apply.",
        ] {
            script = script.replace(
                guest,
                sb.tmp()
                    .join(guest.trim_start_matches('/'))
                    .to_str()
                    .unwrap(),
            );
        }
        let lib = sb.tmp().join("usr/local/lib/awman");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(sb.tmp().join("tmp")).unwrap();
        std::fs::write(lib.join("kit-id"), format!("{}\n", kit.digest)).unwrap();
        let script_path = sb.tmp().join("apply-protocol.sh");
        std::fs::write(&script_path, &script).unwrap();
        (kit, script_path)
    }

    #[test]
    fn apply_protocol_rejects_malformed_stale_and_corrupted_documents_before_policy_writes() {
        let sb = sandbox("engine::sandbox::dsbx::kit::script_tests::apply_protocol_rejects_malformed_stale_and_corrupted_documents_before_policy_writes");
        let (kit, script_path) = private_apply_script(&sb);
        let document = ApplyDocument {
            launch_id: "launch-1".into(),
            agent: "claude".into(),
            kit_digest: kit.digest.clone(),
            files: vec![ApplyFile {
                id: CLAUDE_MEMORY.into(),
                content: b"context".to_vec(),
            }],
        };
        let wire = String::from_utf8(document.encode().unwrap()).unwrap();
        let bad_hash = sha256_hex(b"different");
        let cases = [
            (wire.replace("AWMAN-APPLY 2", "AWMAN-APPLY 1"), 64),
            (wire.replace("agent claude", "agent codex"), 64),
            (wire.replace("file claude-memory", "file unknown"), 64),
            (wire.replace("end\n", "end\nextra\n"), 64),
            (wire.replace("end\n", ""), 64),
            (wire.replace(&kit.digest, &"0".repeat(64)), 3),
            (wire.replace(&sha256_hex(b"context"), &bad_hash), 65),
            (wire.replace("Y29udGV4dA==", "A"), 65),
            (
                wire.replace(
                    "end\n",
                    &format!(
                        "file claude-memory {} Y29udGV4dA==\nend\n",
                        sha256_hex(b"context")
                    ),
                ),
                64,
            ),
        ];
        for (input, code) in cases {
            let out = sb
                .command("/bin/bash")
                .arg(&script_path)
                .stdin(Stdin::Bytes(input.into_bytes()))
                .run();
            assert_eq!(out.exit, Exit::Code(code), "{}", out.stderr_text());
            assert!(!sb.tmp().join("home/agent/.claude/CLAUDE.md").exists());
            assert!(!sb.tmp().join("var/lib/awman/receipt").exists());
        }
        sb.finish();
    }

    #[test]
    fn applies_and_clears_managed_files_checks_symlinks_and_gates_each_receipt() {
        use crate::engine::agent::sbx_contract::{apply_acknowledged, CLAUDE_MANAGED_SETTINGS};
        let sb = sandbox("engine::sandbox::dsbx::kit::script_tests::applies_and_clears_managed_files_checks_symlinks_and_gates_each_receipt");
        let (kit, script) = private_apply_script(&sb);
        let mut document = ApplyDocument {
            launch_id: "launch-one".into(),
            agent: "claude".into(),
            kit_digest: kit.digest,
            files: vec![
                ApplyFile {
                    id: CLAUDE_MEMORY.into(),
                    content: b"context".to_vec(),
                },
                ApplyFile {
                    id: CLAUDE_MANAGED_SETTINGS.into(),
                    content: b"{}".to_vec(),
                },
            ],
        };
        let apply = |document: &ApplyDocument| {
            sb.command("/bin/bash")
                .arg(&script)
                .stdin(Stdin::Bytes(document.encode().unwrap()))
                .run()
        };
        let output = apply(&document);
        output.assert_success();
        assert!(apply_acknowledged(&document, &output.stdout_text()));
        let memory = sb.tmp().join("home/agent/.claude/CLAUDE.md");
        let settings = sb.tmp().join("etc/claude-code/managed-settings.json");
        assert_eq!(std::fs::read(&memory).unwrap(), b"context");
        assert_eq!(std::fs::read(&settings).unwrap(), b"{}");
        let mut gate = sbx_launch_script().to_owned();
        for guest in ["/var/lib/awman", "/usr/local/lib/awman"] {
            gate = gate.replace(
                guest,
                sb.tmp()
                    .join(guest.trim_start_matches('/'))
                    .to_str()
                    .unwrap(),
            );
        }
        let gate_path = sb.tmp().join("gate.sh");
        std::fs::write(&gate_path, gate).unwrap();
        let launch = |id: &str| {
            sb.command("/bin/bash")
                .arg(&gate_path)
                .arg("/bin/true")
                .env("AWMAN_LAUNCH_ID", id)
                .run()
        };
        launch("launch-one").assert_success();
        assert_eq!(launch("other").exit, Exit::Code(78));
        document.launch_id = "launch-two".into();
        document.files.clear();
        apply(&document).assert_success();
        assert!(!memory.exists());
        assert!(!settings.exists());
        assert_eq!(launch("launch-one").exit, Exit::Code(78));
        launch("launch-two").assert_success();
        let outside = sb.work().join("untouched");
        std::fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, &memory).unwrap();
        document.launch_id = "launch-three".into();
        document.files.push(ApplyFile {
            id: CLAUDE_MEMORY.into(),
            content: b"replace".to_vec(),
        });
        assert_eq!(apply(&document).exit, Exit::Code(73));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "keep");
        assert_eq!(launch("launch-three").exit, Exit::Code(78));
        assert_eq!(launch("launch-two").exit, Exit::Code(78));
        sb.finish();
    }
    #[test]
    fn apply_preserves_unowned_and_modified_context_and_refuses_symlink_parents() {
        let sb = sandbox("engine::sandbox::dsbx::kit::script_tests::apply_preserves_unowned_and_modified_context_and_refuses_symlink_parents");
        let (kit, script) = private_apply_script(&sb);
        let memory = sb.tmp().join("home/agent/.claude/CLAUDE.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        let mut doc = ApplyDocument {
            launch_id: "ownership".into(),
            agent: "claude".into(),
            kit_digest: kit.digest,
            files: vec![],
        };
        let apply = |doc: &ApplyDocument| {
            sb.command("/bin/bash")
                .arg(&script)
                .stdin(Stdin::Bytes(doc.encode().unwrap()))
                .run()
        };
        std::fs::write(&memory, "personal instructions").unwrap();
        apply(&doc).assert_success();
        assert_eq!(
            std::fs::read_to_string(&memory).unwrap(),
            "personal instructions"
        );
        doc.files.push(ApplyFile {
            id: CLAUDE_MEMORY.into(),
            content: b"awman context".to_vec(),
        });
        assert_eq!(apply(&doc).exit, Exit::Code(73));
        std::fs::remove_file(&memory).unwrap();
        apply(&doc).assert_success();
        std::fs::write(&memory, "user edited context").unwrap();
        assert_eq!(apply(&doc).exit, Exit::Code(73));
        doc.files.clear();
        assert_eq!(apply(&doc).exit, Exit::Code(73));
        assert_eq!(
            std::fs::read_to_string(&memory).unwrap(),
            "user edited context"
        );
        std::fs::remove_file(&memory).unwrap();
        std::fs::remove_dir(memory.parent().unwrap()).unwrap();
        let outside = sb.work().join("linked-config");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("CLAUDE.md"), "keep repository instructions").unwrap();
        std::os::unix::fs::symlink(&outside, memory.parent().unwrap()).unwrap();
        assert_eq!(apply(&doc).exit, Exit::Code(73));
        doc.files.push(ApplyFile {
            id: CLAUDE_MEMORY.into(),
            content: b"overwrite".to_vec(),
        });
        assert_eq!(apply(&doc).exit, Exit::Code(73));
        assert_eq!(
            std::fs::read_to_string(outside.join("CLAUDE.md")).unwrap(),
            "keep repository instructions"
        );
        sb.finish();
    }
}
