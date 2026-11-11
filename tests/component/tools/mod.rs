//! Shipping-tool contracts run with private state on GNU and BSD userlands.
mod architecture;
use crate::support;
fn root() -> std::path::PathBuf {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .canonicalize()
        .unwrap();
    if cfg!(windows) {
        // Git Bash accepts drive paths, not Rust's verbatim \\?\ spelling.
        root.to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .replace('\\', "/")
            .into()
    } else {
        root
    }
}
fn setup(
    s: &awman_test_support::TestSandbox,
) -> (
    awman_test_support::fakes::FakeCli,
    awman_test_support::fakes::FakeCli,
    std::path::PathBuf,
) {
    let uname = s.install_fake(support::fake_cli_exe(), "uname");
    uname.on(&["-s"]).stdout("Darwin\n");
    let codesign = s.install_fake(support::fake_cli_exe(), "codesign");
    codesign.on(&["--verify"]).exit(0);
    codesign
        .on(&["-d", "--entitlements"])
        .stdout("<plist version=\"1.0\"><dict/></plist>\n");
    codesign
        .on(&["-dvvv"])
        .stderr("Identifier=awman\nSignature=adhoc\nCDHash=abc\n");
    let plutil = s.install_fake(support::fake_cli_exe(), "plutil");
    plutil.on(&["-convert", "json"]).stdout("{}\n");
    let binary = s.work().join("candidate with spaces");
    std::fs::write(&binary, "candidate bytes").unwrap();
    (codesign, plutil, binary)
}
#[test]
fn signature_verification_preserves_exact_candidate_bytes() {
    let s = support::sandbox("tools::signature_verification_preserves_exact_candidate_bytes");
    let (codesign, _plutil, binary) = setup(&s);
    let out = s
        .command("bash")
        .arg(root().join("tools/macos-sign.sh"))
        .arg("--verify")
        .arg(&binary)
        .run();
    out.assert_success();
    let calls = codesign.calls();
    assert!(calls
        .iter()
        .any(|c| c.argv.starts_with(&["--verify".into(), "--strict".into()])));
    assert!(!calls
        .iter()
        .any(|c| c.argv.iter().any(|arg| arg == "--force" || arg == "--sign")));
    assert_eq!(std::fs::read_to_string(binary).unwrap(), "candidate bytes");
}
#[test]
fn entitlements_mismatch_fails_verification() {
    let s = support::sandbox("tools::entitlements_mismatch_fails_verification");
    let (_codesign, plutil, binary) = setup(&s);
    let entitlements = root().join("packaging/macos/awman.entitlements");
    plutil
        .on(&[
            "-convert",
            "json",
            "-o",
            "-",
            entitlements.to_str().unwrap(),
        ])
        .stdout("{\"com.apple.security.hypervisor\":true}\n");
    let out = s
        .command("bash")
        .arg(root().join("tools/macos-sign.sh"))
        .arg("--verify")
        .arg(&binary)
        .run();
    assert!(!out.exit.success(), "mismatched entitlements accepted");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("embedded entitlements differ"),
        "{out:?}"
    );
}
