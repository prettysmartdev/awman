//! Native Docker/Apple mount regression. For all runtimes, including the
//! signed builtin worker host, run scripts/validate-worker-gate.sh.
#[path = "worker_native/probe.rs"]
mod probe;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_worker_workspace_denies_writes_and_output_receipt_requires_guest_mount() {
    use awman::engine::container::ContainerRuntime;
    let Ok(selected) = std::env::var("AWMAN_TEST_WORKER_NATIVE") else {
        eprintln!(
            "BLOCKED: native worker enforcement not requested; this is not native PASS evidence"
        );
        return;
    };
    let image = std::env::var("AWMAN_TEST_WORKER_NATIVE_IMAGE")
        .expect("provide a locally available POSIX-shell image");
    let env = awman::data::config::env::EnvSnapshot::with_overrides(std::env::vars());
    let runtime = match selected.as_str() {
        "docker" => ContainerRuntime::docker_in(env),
        "apple-containers" => ContainerRuntime::apple_in(env),
        "builtin" => {
            panic!("builtin requires a signed worker host: run scripts/validate-worker-gate.sh")
        }
        _ => panic!("unsupported native gate runtime: {selected}"),
    };
    runtime
        .availability()
        .expect("selected native runtime must be available");
    probe::run(std::sync::Arc::new(runtime), &selected, &image).await;
}
