//! Release Docker session on exact attested bytes.
#[test]
#[cfg(target_os = "linux")]
fn exact_asset_runs_a_synthetic_session_in_an_owned_container() {
    crate::support::runtime_smoke::smoke(
        "docker",
        "docker_smoke::exact_asset_runs_a_synthetic_session_in_an_owned_container",
    );
}
