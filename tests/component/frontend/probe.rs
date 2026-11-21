use crate::support;

#[test]
fn help_and_version_are_successful_without_runtime_or_daemon_startup() {
    let s = support::sandbox(
        "frontend::probe::help_and_version_are_successful_without_runtime_or_daemon_startup",
    );
    let help = support::awman(&s).arg("--help").run();
    help.assert_success();
    assert!(help.stdout_text().contains("Usage:"));
    assert!(help.stdout_text().contains("Commands:"));
    let version = support::awman(&s).arg("--version").run();
    version.assert_success();
    assert_eq!(
        version.stdout_text().trim(),
        concat!("awman ", env!("CARGO_PKG_VERSION"))
    );
    let invalid = support::awman(&s).arg("--definitely-invalid-option").run();
    assert!(!invalid.exit.success());
    assert!(invalid.stderr_text().contains("unexpected argument"));
    s.finish();
}
