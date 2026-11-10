use awman::{
    command::commands::api_server::{event_bus::EventBus, owned_runtime::CommandOwnedRuntime},
    data::fs::{api_command_log::CommandStateWriter, api_paths::ApiPaths},
    engine::{
        agent_runtime::{AgentRuntimeEngine, ResolvedAgentOptions},
        container::options::*,
    },
    frontend::api::command_frontend::ApiDispatchFrontend,
};
use std::sync::Arc;

pub async fn run(runtime: Arc<dyn AgentRuntimeEngine>, selected: &str, image: &str) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = ApiPaths::from_root(temp.path().join("api"));
    let session = awman::data::ids::WorkerSessionId::new().to_string();
    for read_only in [false, true] {
        let command = awman::data::ids::CommandId::new().to_string();
        paths.prepare_command_dir(&session, &command).unwrap();
        let writer = CommandStateWriter::new(paths.clone(), &session, &command);
        let owned =
            CommandOwnedRuntime::new(runtime.clone(), &command).with_output_writer(writer.clone());
        let script = if read_only {
            "if touch /workspace/forbidden 2>/dev/null; then exit 71; fi; test ! -e /workspace/forbidden; echo mounted > /awman/output/proof"
        } else {
            "echo writable > /workspace/control; echo mounted > /awman/output/proof"
        };
        let mut options = ResolvedContainerOptions::resolve([
            ContainerOption::Image(ImageRef::new(image)),
            ContainerOption::Entrypoint(Entrypoint::new(["/bin/sh", "-c", script])),
            ContainerOption::Interactive(false),
            ContainerOption::Overlay(OverlaySpec {
                host_path: workspace.clone(),
                container_path: "/workspace".into(),
                permission: if read_only {
                    OverlayPermission::ReadOnly
                } else {
                    OverlayPermission::ReadWrite
                },
            }),
            ContainerOption::Overlay(OverlaySpec {
                host_path: writer.output_dir(),
                container_path: "/awman/output".into(),
                permission: OverlayPermission::ReadWrite,
            }),
        ])
        .unwrap();
        if selected == "builtin" {
            options.builtin_launch = Some(Box::new(
                awman::data::config::builtin_launch::BuiltinLaunchSettings::default(),
            ));
        }
        let instance = owned
            .build(ResolvedAgentOptions::Container(Box::new(options)))
            .unwrap();
        assert_eq!(
            writer.result_contributions().outputs_mounted,
            Some(false),
            "building a plan cannot acknowledge a mount"
        );
        let bus = EventBus::new(1024);
        let frontend = ApiDispatchFrontend::new(
            "exec prompt",
            &["--headless".into(), "probe".into()],
            bus.sender(),
        );
        let mut execution = instance.run_with_frontend(Box::new(frontend)).unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(90), execution.wait())
            .await
            .expect("native launch deadline")
            .unwrap();
        assert_eq!(exit.exit_code, 0, "{selected} read_only={read_only}");
        assert_eq!(writer.result_contributions().outputs_mounted, Some(true));
        assert_eq!(
            std::fs::read_to_string(writer.output_dir().join("proof"))
                .unwrap()
                .trim(),
            "mounted"
        );
    }
    assert!(workspace.join("control").exists(), "positive write control");
    assert!(!workspace.join("forbidden").exists());
    println!("PASS: worker mount probe {selected}");
}
