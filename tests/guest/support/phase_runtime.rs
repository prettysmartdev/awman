// Native phase checks run in the registered driver, which can host worker re-exec.
use awman::data::{
    config::builtin_launch::BuiltinLaunchSettings,
    message::{UserMessage, UserMessageSink},
};
use awman::engine::{
    agent_runtime::{
        execution::AgentExecution,
        frontend::{AgentFrontend, AgentIo, AgentProgress, AgentStatus},
    },
    container::{phase_step::PhaseStepContainerSpec, ContainerRuntime},
};
use std::{io::BufRead, path::Path, time::Duration};

struct Frontend(Option<AgentIo>);
impl UserMessageSink for Frontend {
    fn write_message(&mut self, message: UserMessage) {
        eprintln!("{}", message.text);
    }
    fn replay_queued(&mut self) {}
}
impl AgentFrontend for Frontend {
    fn take_io(&mut self) -> AgentIo {
        self.0.take().unwrap()
    }
    fn report_status(&mut self, _: AgentStatus) {}
    fn report_progress(&mut self, _: AgentProgress) {}
    fn grace_timeout(&self) -> Duration {
        Duration::from_millis(10)
    }
    fn stuck_timeout(&self) -> Duration {
        Duration::from_secs(120)
    }
}
fn launch(
    runtime: &ContainerRuntime,
    tag: &str,
    work: &Path,
    launch: &BuiltinLaunchSettings,
    command: &str,
    retain_stopped: bool,
) -> Result<(AgentExecution, Vec<tokio::task::JoinHandle<()>>), String> {
    let env = Default::default();
    let instance = if retain_stopped {
        use awman::engine::container::options::{Entrypoint, ImageRef, ResolvedContainerOptions};
        runtime.build(ResolvedContainerOptions {
            image: Some(ImageRef::new(tag)),
            entrypoint: Some(Entrypoint::new(["/bin/sh", "-c", command])),
            builtin_launch: Some(Box::new(launch.clone())),
            startup_grace: Some(Duration::MAX),
            full_transcript: true,
            host_env: Some(runtime.env().clone()),
            interactive: true,
            remove_on_exit: false,
            ..Default::default()
        })
    } else {
        runtime.build_phase_step_in(
            &PhaseStepContainerSpec {
                builtin_launch: Some(launch),
                image: tag,
                workdir: work,
                overlays: &[],
                env: &env,
                command,
                step_env: None,
            },
            runtime.env(),
        )
    }
    .map_err(|e| e.to_string())?;
    let (stdout, mut out) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, mut err) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let drains = vec![
        tokio::spawn(async move { while out.recv().await.is_some() {} }),
        tokio::spawn(async move { while err.recv().await.is_some() {} }),
    ];
    let execution = instance
        .run_with_frontend(Box::new(Frontend(Some(AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            initial_size: Some((80, 24)),
            resize: None,
        }))))
        .map_err(|e| e.to_string())?;
    Ok((execution, drains))
}

async fn wait_for_shutdown(execution: &mut AgentExecution) -> Result<(), String> {
    let outcome = tokio::time::timeout(Duration::from_secs(30), execution.wait())
        .await
        .map_err(|_| "shutdown deadline")?;
    match outcome {
        Ok(_) => Ok(()),
        // Stopping the VM may close the job transport before an exit frame.
        // Only an explicit, confirmed shutdown is sufficient in that case.
        Err(awman::engine::error::EngineError::ExecutionStreamFailed { stopped: true, .. }) => {
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

pub async fn run(
    runtime: &ContainerRuntime,
    scenario: &str,
    tag: &str,
    work: &Path,
    settings: &BuiltinLaunchSettings,
) -> Result<(), String> {
    if scenario == "phase-quiet-transcript" {
        // Quiet longer than the production first-byte polling interval, with a
        // frontend grace of 10 ms. No host provider executable is involved.
        let (mut execution, drains) = launch(runtime, tag, work, settings,
            "sleep 2; i=0; while [ $i -lt 20000 ]; do printf '%06d:abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij\\n' $i; i=$((i+1)); done; exit 37", false)?;
        let tail = execution.output_tail().ok_or("missing transcript")?;
        let mut startup_events = execution.subscribe_stuck();
        let exit = tokio::time::timeout(Duration::from_secs(90), execution.wait())
            .await
            .map_err(|_| "phase deadline")?
            .map_err(|e| e.to_string())?;
        if exit.exit_code != 37 {
            return Err(format!("phase exit {}", exit.exit_code));
        }
        while let Ok(event) = startup_events.try_recv() {
            if matches!(
                event,
                awman::engine::agent_runtime::execution::StuckEvent::StartupGraceExpired
            ) {
                return Err("quiet phase incorrectly reached first-byte grace".into());
            }
        }
        for drain in drains {
            drain.await.map_err(|e| e.to_string())?;
        }
        let mut reader = tail.transcript_reader().map_err(|e| e.to_string())?;
        let mut file =
            awman::data::fs::private_files::create_private_file_new(&work.join("phase-full.log"))
                .map_err(|e| e.to_string())?;
        let bytes = std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
        if bytes <= 1024 * 1024 {
            return Err(format!("transcript too short: {bytes}"));
        }
        drop(file);
        let reader = std::io::BufReader::new(tail.transcript_reader().map_err(|e| e.to_string())?);
        let mut count = 0;
        for (i, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| e.to_string())?;
            let expected = format!(
                "{i:06}:abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij"
            );
            if line.trim_end_matches('\r') != expected {
                return Err(format!("transcript order/content at {i}"));
            }
            count += 1;
        }
        if count != 20000 {
            return Err(format!("transcript lines: {count}"));
        }
        println!("PHASE_EXIT\t37\nTRANSCRIPT_LINES\t{count}\nTRANSCRIPT_BYTES\t{bytes}");
        // Disabled startup grace must still permit an explicit cancellation.
        let (mut execution, drains) = launch(runtime, tag, work, settings, "sleep 120", false)?;
        execution
            .cancel_handle()
            .ok_or("missing cancellation")?
            .cancel()
            .map_err(|e| e.to_string())?;
        wait_for_shutdown(&mut execution).await?;
        for drain in drains {
            drain.await.map_err(|e| e.to_string())?;
        }
        println!("QUIET_CANCELLED\ttrue");
    } else if scenario == "phase-stop-remove" {
        let (mut execution, drains) = launch(runtime, tag, work, settings, "sleep 120", true)?;
        let handle = runtime
            .list_running_all()
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or("no running phase")?;
        runtime.stop(&handle).map_err(|e| e.to_string())?;
        runtime.stop(&handle).map_err(|e| e.to_string())?;
        let stopped = runtime.list_stopped().map_err(|e| e.to_string())?;
        if !stopped.iter().any(|h| h.id == handle.id) {
            return Err("stopped instance was removed".into());
        }
        runtime
            .remove_container(&handle.id)
            .map_err(|e| e.to_string())?;
        runtime
            .remove_container(&handle.id)
            .map_err(|e| e.to_string())?;
        wait_for_shutdown(&mut execution).await?;
        for drain in drains {
            drain.await.map_err(|e| e.to_string())?;
        }
        println!("STOP_GET_REMOVE\ttrue");
    } else {
        return Err(format!("unknown phase scenario {scenario}"));
    }
    Ok(())
}
