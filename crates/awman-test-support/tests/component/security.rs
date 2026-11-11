use super::{expect_panic, sandbox};

#[cfg(unix)]
#[test]
fn generic_registration_cannot_authorize_host_keychain_or_agent_names() {
    let parent = tempfile::tempdir().unwrap();
    for name in ["security", "pbcopy", "claude"] {
        let exe = parent.path().join(name);
        std::fs::copy("/bin/true", &exe).unwrap();
        let mut s = sandbox("security::registered_forbidden_tools");
        s.allow_tool(&exe);
        let text = expect_panic(|| {
            s.command(&exe).run();
        });
        assert!(text.contains("REFUSED"), "{text}");
        s.finish();
    }
}

#[cfg(unix)]
#[test]
fn registered_interpreters_still_check_their_arguments() {
    let mut s = sandbox("security::registered_interpreters");
    s.allow_tool(std::path::Path::new("/bin/sh"));
    let text = expect_panic(|| {
        s.command("/bin/sh")
            .args(["-c", "security find-generic-password"])
            .run();
    });
    assert!(text.contains("REFUSED"), "{text}");
    s.finish();
}

#[cfg(target_os = "linux")]
#[test]
fn descendants_cannot_create_new_sessions() {
    let s = sandbox("security::descendants_cannot_detach");
    let out = s.command("/usr/bin/perl").args(["-e", "use POSIX qw(setsid); defined(setsid()) && setsid() >= 0 and die 'detached'; $!{EPERM} or die 'wrong error'; print qq(contained\n);"]).run();
    out.assert_success();
    assert_eq!(out.stdout_text(), "contained\n");
    s.finish();
}
