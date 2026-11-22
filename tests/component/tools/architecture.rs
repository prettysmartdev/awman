use crate::support;
use awman_test_support::Exit;

#[test]
fn architecture_gate_detects_cross_layer_calls_and_scanner_failure() {
    let mut s = support::sandbox("tools::architecture_gate");
    let tree = s.work().join("source");
    for dir in [
        "src/data",
        "src/engine",
        "src/command",
        "src/frontend",
        "crates/awman-host-shims/src",
    ] {
        std::fs::create_dir_all(tree.join(dir)).unwrap();
        std::fs::write(tree.join(dir).join("lib.rs"), "pub struct Value;\n").unwrap();
    }
    s.env_mut().set("ARCH_LINT_ROOT", &tree);
    std::fs::create_dir_all(tree.join("tools")).unwrap();
    std::fs::write(tree.join("tools/env-lint-allowlist.txt"), "").unwrap();
    let script = super::root().join("tools/architecture-lint.sh");
    s.command("bash").arg(&script).run().assert_success();
    for (path, source) in [
        ("src/data/lib.rs", "use crate::engine::git::Git;\n"),
        (
            "src/engine/lib.rs",
            "use crate::{data::config, command::dispatch};\n",
        ),
        (
            "src/frontend/lib.rs",
            "fn bad(engines: Engines) { engines.git_engine.clone_repo(); }\n",
        ),
    ] {
        std::fs::write(tree.join(path), source).unwrap();
        let result = s.command("bash").arg(&script).run();
        assert_eq!(result.exit, Exit::Code(1), "{path}: {result:?}");
        std::fs::write(tree.join(path), "pub struct Value;\n").unwrap();
    }
    let grep = s.install_fake(support::fake_cli_exe(), "grep");
    grep.on(&[]).exit(2).stderr("scanner unavailable\n");
    let result = s.command("bash").arg(script).run();
    assert_eq!(result.exit, Exit::Code(2), "{result:?}");
    assert!(result.stderr_text().contains("SCANNER ERROR"));
}
