use super::*;
use crate::command::dispatch::catalogue::CommandCatalogue;
use std::sync::Mutex;

const BUILD: &str = env!("CARGO_PKG_VERSION");

fn running(engine: EngineProbe, jobs: usize, build: &str, proto: Option<&str>) -> DaemonProbe {
    DaemonProbe::Running {
        pid: 1234,
        build: build.into(),
        frontend_protocol: proto.map(String::from),
        engine,
        running_jobs: jobs,
    }
}

fn good(engine: EngineProbe) -> DaemonProbe {
    running(engine, 0, BUILD, Some(FRONTEND_SUBPROTOCOL))
}

fn inputs(
    backend: TuiBackend,
    requirement: HostRequirement,
    engine_on: bool,
    daemon: DaemonProbe,
) -> HandoffInputs {
    HandoffInputs {
        backend,
        requirement,
        engine_configured_on: engine_on,
        daemon,
        this_build: BUILD.into(),
    }
}

fn required() -> HostRequirement {
    HostRequirement::Required(Annotations::new())
}

fn d(enable: EngineEnable, on_failure: Fallback) -> Decision {
    Decision::Daemon { enable, on_failure }
}

fn refused(on_failure: Fallback) -> impl Fn(&Decision) -> bool {
    move |got| matches!(got, Decision::Refused { on_failure: f, .. } if *f == on_failure)
}

#[test]
fn builtin_wins_over_everything() {
    for req in [HostRequirement::None, required()] {
        for daemon in [
            DaemonProbe::NotRunning,
            good(EngineProbe::Persistent),
            DaemonProbe::Unreachable("x".into()),
        ] {
            for engine_on in [false, true] {
                let i = inputs(TuiBackend::Builtin, req.clone(), engine_on, daemon.clone());
                assert_eq!(decide(&i), Decision::Builtin);
            }
        }
    }
}

#[test]
fn table_by_backend_requirement_engine_and_daemon_state() {
    use EngineEnable::*;
    use Fallback::*;
    let none = HostRequirement::None;
    let optional = HostRequirement::Optional(Annotations::new());
    // (backend, requirement, engine configured on, daemon, expected)
    let rows: Vec<(TuiBackend, HostRequirement, bool, DaemonProbe, Decision)> = vec![
        // daemon: always daemon, failure exits
        (
            TuiBackend::Daemon,
            none.clone(),
            false,
            DaemonProbe::NotRunning,
            d(StartDaemon, Exit),
        ),
        (
            TuiBackend::Daemon,
            none.clone(),
            false,
            good(EngineProbe::Off {
                allow_ephemeral: true,
            }),
            d(Ephemeral, Exit),
        ),
        (
            TuiBackend::Daemon,
            none.clone(),
            false,
            good(EngineProbe::Ephemeral),
            d(AlreadyOn, Exit),
        ),
        (
            TuiBackend::Daemon,
            none.clone(),
            true,
            good(EngineProbe::Persistent),
            d(AlreadyOn, Exit),
        ),
        (
            TuiBackend::Daemon,
            required(),
            false,
            good(EngineProbe::Off {
                allow_ephemeral: true,
            }),
            d(Ephemeral, Exit),
        ),
        // auto + host requires: daemon, failure falls back
        (
            TuiBackend::Auto,
            required(),
            false,
            DaemonProbe::NotRunning,
            d(StartDaemon, Builtin),
        ),
        (
            TuiBackend::Auto,
            required(),
            false,
            good(EngineProbe::Off {
                allow_ephemeral: true,
            }),
            d(Ephemeral, Builtin),
        ),
        (
            TuiBackend::Auto,
            required(),
            false,
            good(EngineProbe::Ephemeral),
            d(AlreadyOn, Builtin),
        ),
        (
            TuiBackend::Auto,
            required(),
            true,
            good(EngineProbe::Persistent),
            d(AlreadyOn, Builtin),
        ),
        // auto + engine persistently on
        (
            TuiBackend::Auto,
            none.clone(),
            true,
            DaemonProbe::NotRunning,
            d(StartDaemon, Builtin),
        ),
        (
            TuiBackend::Auto,
            none.clone(),
            false,
            good(EngineProbe::Persistent),
            d(AlreadyOn, Builtin),
        ),
        // auto otherwise: builtin (an ephemeral engine someone else enabled does not count)
        (
            TuiBackend::Auto,
            none.clone(),
            false,
            DaemonProbe::NotRunning,
            Decision::Builtin,
        ),
        (
            TuiBackend::Auto,
            optional,
            false,
            DaemonProbe::NotRunning,
            Decision::Builtin,
        ),
        (
            TuiBackend::Auto,
            none.clone(),
            false,
            good(EngineProbe::Ephemeral),
            Decision::Builtin,
        ),
        (
            TuiBackend::Auto,
            none,
            false,
            good(EngineProbe::Off {
                allow_ephemeral: true,
            }),
            Decision::Builtin,
        ),
    ];
    for (backend, req, engine_on, daemon, expected) in rows {
        let got = decide(&inputs(backend, req.clone(), engine_on, daemon.clone()));
        assert_eq!(
            got, expected,
            "{backend:?} {req:?} engine_on={engine_on} {daemon:?}"
        );
    }
}

#[test]
fn ephemeral_forbidden_and_draining_are_refused_with_the_backend_fallback() {
    for (backend, fb) in [
        (TuiBackend::Daemon, Fallback::Exit),
        (TuiBackend::Auto, Fallback::Builtin),
    ] {
        for engine in [
            EngineProbe::Off {
                allow_ephemeral: false,
            },
            EngineProbe::Draining,
        ] {
            let got = decide(&inputs(backend, required(), false, good(engine.clone())));
            assert!(refused(fb)(&got), "{backend:?} {engine:?} -> {got:?}");
        }
    }
}

#[test]
fn unreachable_daemon_is_refused() {
    let got = decide(&inputs(
        TuiBackend::Daemon,
        HostRequirement::None,
        false,
        DaemonProbe::Unreachable("boom".into()),
    ));
    assert!(refused(Fallback::Exit)(&got));
    assert!(matches!(&got, Decision::Refused { reason, .. } if reason.contains("boom")));
}

#[test]
fn version_mismatch_restarts_idle_and_busy_daemons_and_is_never_driven_silently() {
    let mismatches = [
        ("0.0.0-old", Some(FRONTEND_SUBPROTOCOL)),
        (BUILD, None),
        (BUILD, Some("awman.frontend.v0")),
    ];
    for (build, proto) in mismatches {
        for jobs in [0usize, 3] {
            for (backend, fb) in [
                (TuiBackend::Daemon, Fallback::Exit),
                (TuiBackend::Auto, Fallback::Builtin),
            ] {
                let daemon = running(EngineProbe::Persistent, jobs, build, proto);
                let got = decide(&inputs(backend, required(), false, daemon));
                assert_eq!(
                    got,
                    Decision::RestartStaleDaemon {
                        running_jobs: jobs,
                        then: Box::new(d(EngineEnable::StartDaemon, fb)),
                        on_failure: fb,
                    }
                );
            }
        }
    }
}

#[test]
fn stale_daemon_is_ignored_when_builtin_is_chosen() {
    let daemon = running(
        EngineProbe::Off {
            allow_ephemeral: true,
        },
        5,
        "old",
        None,
    );
    assert_eq!(
        decide(&inputs(
            TuiBackend::Auto,
            HostRequirement::None,
            false,
            daemon.clone()
        )),
        Decision::Builtin
    );
    assert_eq!(
        decide(&inputs(TuiBackend::Builtin, required(), true, daemon)),
        Decision::Builtin
    );
}

#[test]
fn notices_and_descriptions() {
    assert_eq!(
        fallback_notice("daemon failed to start: x"),
        "engine: unavailable: daemon failed to start: x"
    );
    assert!(restart_advice(3).contains("running 3 jobs"));
    assert!(restart_advice(1).contains("running 1 job."));
    assert_eq!(describe(&BackendSelection::Builtin), "backend: builtin");
}

#[test]
fn clap_rejects_removed_flags() {
    let cmd = CommandCatalogue::get().build_clap_command();
    for flag in ["--engine", "--integration", "--no-integrations"] {
        for argv in [vec!["awman", flag], vec!["awman", "chat", flag]] {
            assert!(
                cmd.clone().try_get_matches_from(&argv).is_err(),
                "{argv:?} must be rejected"
            );
        }
    }
}

// ── driver, over a fake link ──

#[derive(Default)]
struct FakeLink {
    probes: Mutex<Vec<DaemonProbe>>,
    calls: Mutex<Vec<&'static str>>,
    start_err: Option<String>,
    enable_err: Option<EnableError>,
}

impl FakeLink {
    fn with(probes: Vec<DaemonProbe>) -> Self {
        Self {
            probes: Mutex::new(probes),
            ..Default::default()
        }
    }
}

#[async_trait::async_trait]
impl DaemonLink for FakeLink {
    async fn probe(&self) -> DaemonProbe {
        let mut p = self.probes.lock().unwrap();
        if p.len() > 1 {
            p.remove(0)
        } else {
            p[0].clone()
        }
    }
    async fn start(&self) -> Result<(), String> {
        self.calls.lock().unwrap().push("start");
        self.start_err.clone().map_or(Ok(()), Err)
    }
    async fn enable_engine_ephemeral(&self) -> Result<(), EnableError> {
        self.calls.lock().unwrap().push("enable");
        self.enable_err.clone().map_or(Ok(()), Err)
    }
    async fn stop(&self) -> Result<(), String> {
        self.calls.lock().unwrap().push("stop");
        Ok(())
    }
    fn endpoint(&self) -> Result<LocalEndpoint, String> {
        Err("fake link has no endpoint".into())
    }
}

struct Answer(bool, Mutex<Vec<usize>>);
#[async_trait::async_trait]
impl HandoffPrompter for Answer {
    async fn confirm_daemon_restart(&self, jobs: usize) -> bool {
        self.1.lock().unwrap().push(jobs);
        self.0
    }
}

fn handoff(
    config: EffectiveConfig,
    link: Arc<FakeLink>,
    yes: bool,
) -> (EngineHandoff, Arc<Answer>) {
    let prompter = Arc::new(Answer(yes, Mutex::new(Vec::new())));
    let env = EnvSnapshot::empty();
    (
        EngineHandoff::with_link(
            env,
            config,
            IntegrationRegistry::with(vec![]),
            link,
            prompter.clone(),
        ),
        prompter,
    )
}

fn config_with(pairs: &[(&str, serde_json::Value)]) -> EffectiveConfig {
    use crate::data::config::effective::ConfigLayers;
    use crate::data::config::overrides::ConfigOverrides;
    let set = ConfigOverrides::new(
        pairs
            .iter()
            .map(|(p, v)| (p.to_string(), v.clone()))
            .collect(),
    );
    EffectiveConfig::resolve(
        ConfigLayers {
            set,
            ..Default::default()
        },
        Default::default(),
        EnvSnapshot::empty(),
    )
    .unwrap()
}

#[tokio::test]
async fn builtin_never_touches_the_daemon() {
    let link = Arc::new(FakeLink::with(vec![DaemonProbe::NotRunning]));
    let cfg = config_with(&[("tui.backend", "builtin".into())]);
    let (h, _) = handoff(cfg, link.clone(), true);
    let r = h.resolve().await.unwrap();
    assert!(matches!(r.selection, BackendSelection::Builtin));
    assert!(r.notice.is_none() && link.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn auto_without_requirement_is_builtin_without_a_notice() {
    let link = Arc::new(FakeLink::with(vec![DaemonProbe::NotRunning]));
    let (h, _) = handoff(EffectiveConfig::default(), link.clone(), true);
    let r = h.resolve().await.unwrap();
    assert!(matches!(r.selection, BackendSelection::Builtin) && r.notice.is_none());
    assert!(link.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn daemon_backend_failure_exits_nonzero_and_auto_falls_back_with_a_notice() {
    let mut link = FakeLink::with(vec![DaemonProbe::NotRunning]);
    link.start_err = Some("permission denied".into());
    let link = Arc::new(link);
    let cfg = config_with(&[("tui.backend", "daemon".into())]);
    let (h, _) = handoff(cfg, link.clone(), true);
    let err = h.resolve().await.unwrap_err();
    assert_eq!(err.exit_code, 1);
    assert_eq!(
        err.message,
        "engine: unavailable: daemon failed to start: permission denied"
    );

    let cfg = config_with(&[("daemon.features.engine.enabled", true.into())]);
    let (h, _) = handoff(cfg, link, true);
    let r = h.resolve().await.unwrap();
    assert!(matches!(r.selection, BackendSelection::Builtin));
    assert_eq!(
        r.notice.as_deref(),
        Some("engine: unavailable: daemon failed to start: permission denied")
    );
}

#[tokio::test]
async fn ephemeral_not_allowed_is_reported() {
    let mut link = FakeLink::with(vec![good(EngineProbe::Off {
        allow_ephemeral: true,
    })]);
    link.enable_err = Some(EnableError::NotAllowed);
    let cfg = config_with(&[("tui.backend", "daemon".into())]);
    let (h, _) = handoff(cfg, Arc::new(link), true);
    let err = h.resolve().await.unwrap_err();
    assert!(err.message.contains("allowEphemeral"));
}

#[tokio::test]
async fn idle_stale_daemon_restarts_silently_and_busy_one_asks() {
    let stale = |jobs| running(EngineProbe::Persistent, jobs, "old", None);
    let cfg = || config_with(&[("tui.backend", "daemon".into())]);

    let link = Arc::new(FakeLink::with(vec![stale(0)]));
    let (h, prompter) = handoff(cfg(), link.clone(), false);
    let _ = h.resolve().await; // endpoint() fails in the fake; only the calls matter
    assert_eq!(*link.calls.lock().unwrap(), ["stop", "start"]);
    assert!(
        prompter.1.lock().unwrap().is_empty(),
        "idle restart must not prompt"
    );

    let link = Arc::new(FakeLink::with(vec![stale(2)]));
    let (h, prompter) = handoff(cfg(), link.clone(), false);
    let err = h.resolve().await.unwrap_err();
    assert_eq!(err.exit_code, EXIT_STALE_DAEMON);
    assert_eq!(*prompter.1.lock().unwrap(), [2]);
    assert!(
        link.calls.lock().unwrap().is_empty(),
        "declined: nothing is stopped"
    );

    let link = Arc::new(FakeLink::with(vec![stale(2)]));
    let (h, _) = handoff(cfg(), link.clone(), true);
    let _ = h.resolve().await;
    assert_eq!(*link.calls.lock().unwrap(), ["stop", "start"]);
}

#[tokio::test]
async fn declined_restart_under_auto_falls_back_to_builtin() {
    let link = Arc::new(FakeLink::with(vec![running(
        EngineProbe::Persistent,
        4,
        "old",
        None,
    )]));
    let (h, _) = handoff(config_with(&[]), link, false);
    let r = h.resolve().await.unwrap();
    assert!(matches!(r.selection, BackendSelection::Builtin));
    assert!(r.notice.unwrap().starts_with("engine: unavailable:"));
}
