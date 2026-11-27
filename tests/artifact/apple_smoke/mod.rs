//! Release Apple Containers session on exact attested bytes.
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn exact_asset_runs_a_synthetic_session_in_an_owned_container() {
    crate::support::runtime_smoke::smoke(
        "apple",
        "apple_smoke::exact_asset_runs_a_synthetic_session_in_an_owned_container",
    );
}
