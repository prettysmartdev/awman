use crate::support;

/// All policies run concurrently against an owned loopback endpoint. The
/// permissive VM is the positive control; no registry or Internet is involved.
#[test]
fn host_port_authorization_is_private_to_each_vm() {
    let s = support::sandbox("network::host_port_authorization_is_private_to_each_vm");
    let archive = support::fixture(&s);
    support::watch(&s);
    let registry = awman_test_support::fakes::FakeRegistry::start(&s);
    let port = registry.addr().port();
    let fetch = format!("wget -T 5 -O /tmp/response http://host.builtin.internal:{port}/v2/");
    // A missing wget fails both probes, including the positive control.
    let plan = s.work().join("host-ports.tsv");
    std::fs::write(&plan, format!(
        "public:{port}\t1\t384\tauthorized\tset -eu; command -v wget; {fetch}; echo authorized-ok\n\
         allowlist\t1\t384\tdenied\tset -eu; command -v wget; if {fetch}; then exit 71; fi; echo denied-ok\n\
         none\t1\t384\tnone\tset -eu; command -v wget; if {fetch}; then exit 72; fi; echo none-ok\n"
    )).unwrap();
    let out = support::checked(
        s.command(support::guest_net_driver_exe())
            .arg("--state-dir")
            .arg(support::state(&s))
            .arg("--archive")
            .arg(archive)
            .args(["--tag", "awman-test/guest:fixture", "--work"])
            .arg(s.work())
            .arg("--plan")
            .arg(plan)
            .run(),
    );
    for name in ["authorized", "denied", "none"] {
        assert!(
            out.lines().any(|line| line == format!("EXIT\t{name}\t0")),
            "{out}"
        );
        let text = std::fs::read_to_string(s.work().join(format!("{name}.stdout"))).unwrap();
        assert!(text.contains(&format!("{name}-ok")), "{text}");
    }
    assert!(out.contains("LEFTOVER\t0"), "{out}");
    assert_eq!(
        registry.requests().len(),
        1,
        "denied VM reached the host endpoint"
    );
}

#[test]
fn network_none_has_no_egress_interface_or_routes() {
    let s = support::sandbox("network::network_none_has_no_egress_interface_or_routes");
    let archive = support::fixture(&s);
    let plan = s.work().join("network.json");
    std::fs::write(&plan, "none\t1\t512\tisolated\tset -eu; test $(tail -n +2 /proc/net/route | wc -l) -eq 0; for i in /sys/class/net/*; do [ ${i##*/} = lo ] || test $(( $(cat $i/flags) & 1 )) -eq 0; done; echo network-isolated\n").unwrap();
    support::watch(&s);
    let out = support::checked(
        s.command(support::guest_net_driver_exe())
            .arg("--state-dir")
            .arg(support::state(&s))
            .arg("--archive")
            .arg(archive)
            .args(["--tag", "awman-test/guest:fixture", "--work"])
            .arg(s.work())
            .arg("--plan")
            .arg(plan)
            .run(),
    );
    assert!(out.contains("LEFTOVER\t0"), "{out}");
    assert!(std::fs::read_to_string(s.work().join("isolated.stdout"))
        .unwrap()
        .contains("network-isolated"));
}
