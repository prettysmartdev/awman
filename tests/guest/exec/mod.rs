use crate::support;
#[test]
fn exit_code_and_streams_survive_the_vm_boundary() {
    let s = support::sandbox("exec::exit_code_and_streams_survive_the_vm_boundary");
    let out = support::scenario(&s, "exit-code");
    assert!(out.contains("EXIT\t37"), "{out}");
    assert_eq!(
        std::fs::read_to_string(s.work().join("exit-code.stdout")).unwrap(),
        "out-marker"
    );
    assert_eq!(
        std::fs::read_to_string(s.work().join("exit-code.stderr")).unwrap(),
        "err-marker"
    );
}
#[test]
fn cancellation_removes_only_the_owned_vm() {
    let s = support::sandbox("exec::cancellation_removes_only_the_owned_vm");
    let out = support::scenario(&s, "cancel");
    assert!(out.contains("RUNNING_AFTER\t0"), "{out}");
}
#[test]
fn acp_is_binary_clean_and_stdin_eof_finishes() {
    let s = support::sandbox("exec::acp_is_binary_clean_and_stdin_eof_finishes");
    let out = support::scenario(&s, "acp-framing");
    assert!(out.contains("ACP_ECHO_EQUAL\ttrue"), "{out}");
    assert!(out.contains("EXIT\t0"), "{out}");
}
#[test]
fn mounts_credentials_and_atomic_refresh_reach_the_guest() {
    let s = support::sandbox("exec::mounts_credentials_and_atomic_refresh_reach_the_guest");
    let out = support::scenario(&s, "mounts");
    assert!(out.contains("HOST_WRITEBACK\tguest"), "{out}");
    let stdout = std::fs::read_to_string(s.work().join("mounts.stdout")).unwrap();
    support::transcript(&stdout, "guest-checks.sh", "guest-suite-complete");
}
#[test]
fn credential_refresh_reaches_multiple_consumers() {
    let s = support::sandbox("exec::credential_refresh_reaches_multiple_consumers");
    support::scenario(&s, "refresh-multi");
    for n in 0..2 {
        assert_eq!(
            std::fs::read_to_string(s.work().join(format!("refresh-multi-{n}.stdout")))
                .unwrap()
                .trim(),
            "seen-v2"
        );
    }
}
#[test]
fn vcpu_and_memory_limits_reach_the_guest() {
    let s = support::sandbox("exec::vcpu_and_memory_limits_reach_the_guest");
    support::scenario(&s, "limits");
    for (file, cpu, min, max) in [
        ("limits-default", 1, 256 * 1024, 512 * 1024),
        ("limits", 2, 384 * 1024, 768 * 1024),
    ] {
        let text = std::fs::read_to_string(s.work().join(format!("{file}.stdout"))).unwrap();
        let values: Vec<u64> = text
            .lines()
            .map(|v| v.trim().parse().expect("guest resource value"))
            .collect();
        assert_eq!(values[0], cpu);
        assert!(values[1] > min && values[1] <= max, "{text}");
    }
}
#[test]
fn pty_resize_is_observed_in_the_guest() {
    let s = support::sandbox("exec::pty_resize_is_observed_in_the_guest");
    let out = support::scenario(&s, "pty-resize");
    assert!(out.contains("24 80") && out.contains("30 100"), "{out}");
}
#[test]
fn reattachment_shares_streams_without_killing_the_owner() {
    let s = support::sandbox("exec::reattachment_shares_streams_without_killing_the_owner");
    let out = support::scenario(&s, "reattach");
    assert!(
        out.contains("REATTACH_BOTH_SAW\ttrue") && out.contains("EXIT\t5"),
        "{out}"
    );
}
#[test]
fn workspace_readonly_and_output_receipts_require_real_mounts() {
    let s = support::sandbox("exec::workspace_readonly_and_output_receipts_require_real_mounts");
    support::scenario(&s, "worker-mount");
}

#[test]
fn explicit_image_overrides_preserve_non_root_identity() {
    let s = support::sandbox("exec::explicit_image_overrides_preserve_non_root_identity");
    support::scenario(&s, "image-overrides");
    let text = std::fs::read_to_string(s.work().join("image-overrides.stdout")).unwrap();
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            "1234",
            "1234",
            "/tmp",
            "/home/probe",
            "runtime override",
            "added"
        ]
    );
}

#[test]
fn concurrent_vm_stop_leaves_other_owners_running() {
    let s = support::sandbox("exec::concurrent_vm_stop_leaves_other_owners_running");
    let out = support::scenario(&s, "multi-vm");
    assert!(out.contains("RUNNING_DURING\t3"), "{out}");
    assert!(
        out.contains("1:0:vm-1") && out.contains("2:0:vm-2"),
        "{out}"
    );
    assert!(out.contains("RUNNING_AFTER\t0"), "{out}");
}

#[test]
fn failed_guest_workload_allows_repeated_kernel_boot() {
    let s = support::sandbox("exec::failed_guest_workload_allows_repeated_kernel_boot");
    let out = support::scenario(&s, "kernel-repeat");
    assert_eq!(
        out.lines().filter(|l| *l == "ROUND_REMAINING\t0").count(),
        3,
        "{out}"
    );
    for round in 0..3 {
        let text = std::fs::read_to_string(s.work().join(format!("kernel-repeat-{round}.stdout")))
            .unwrap();
        assert!(text.contains(&format!("round-{round}")), "{text}");
    }
}
