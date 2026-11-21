//! Apply only live leaves; restart-only settings remain the initial values.
use super::*;
use crate::command::daemon_api::core::{at, changed_paths, ReloadResponse};
use crate::data::config::{ConfigFile, ConfigLayers, FileKind, FlagConfig};
impl DaemonRuntime {
    async fn rollback_config_starts(&self, started: &[FeatureName]) {
        for feature in started {
            let supervisor = &self.supervisors[feature];
            supervisor.disable(StopMode::Now).await;
            supervisor.wait_stopped().await;
        }
        self.preparing.write().unwrap().clear();
    }
    pub async fn reload_config(self: &Arc<Self>) -> Result<ReloadResponse, CommandError> {
        let runtime = self.clone();
        tokio::spawn(async move { runtime.reload_owned().await })
            .await
            .map_err(|e| CommandError::Other(e.to_string()))?
    }
    async fn reload_owned(&self) -> Result<ReloadResponse, CommandError> {
        let _changes = self.changes.lock().await;
        if self.core.shutdown().is_cancelled() || self.exit.is_cancelled() {
            return Err(CommandError::Other("daemon is shutting down".into()));
        }
        let env = self.core.env().clone();
        let candidate = EffectiveConfig::resolve(
            ConfigLayers::load(&env, None, self.initial_config.layers().set.clone())?,
            FlagConfig::default(),
            env.clone(),
        )?;
        let current = self.config();
        let mut response = ReloadResponse::default();
        let mut live = current.value().clone();
        for key in crate::data::config::fields::CONFIG_KEYS {
            let changed = changed_paths(
                if key.live {
                    current.value()
                } else {
                    self.initial_config.value()
                },
                candidate.value(),
                key.path,
            );
            if key.live {
                for path in changed {
                    let parts: Vec<_> = path.split('.').collect();
                    if parts.len() == 5
                        && parts[..3] == ["daemon", "network", "listeners"]
                        && current.value()["daemon"]["network"]["listeners"]
                            .get(parts[3])
                            .is_none()
                    {
                        response.restart_required.push(path);
                        continue;
                    }
                    crate::data::config::config_json::set_config_field(
                        &mut live,
                        &path,
                        at(candidate.value(), &path)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    );
                    response.applied.push(path);
                }
            } else {
                response.restart_required.extend(changed);
            }
        }
        let live = EffectiveConfig::resolve(
            ConfigLayers {
                global: ConfigFile::from_value(live, FileKind::Global)?,
                ..Default::default()
            },
            FlagConfig::default(),
            env,
        )?;
        let mut acknowledgement = self.network.applied.subscribe();
        self.apply_config_locked(live).await?;
        if self.network.hosting.load(Ordering::Acquire) {
            let generation = self.network.tx.borrow().generation;
            let applied = async {
                loop {
                    let (seen, effects) = acknowledgement.borrow_and_update().clone();
                    if seen >= generation {
                        return Ok::<_, tokio::sync::watch::error::RecvError>(effects);
                    }
                    acknowledgement.changed().await?;
                }
            };
            if let Ok(Ok(effects)) = tokio::time::timeout(Duration::from_secs(5), applied).await {
                response.effects = effects;
            }
        }
        Ok(response)
    }
    pub(crate) async fn apply_config_locked(
        &self,
        live: EffectiveConfig,
    ) -> Result<(), CommandError> {
        let settings = live.daemon();
        crate::data::config::network::validate_listeners(&settings.network.listeners)
            .map_err(|e| CommandError::Other(format!("invalid listeners: {e:?}")))?;
        if settings.squad.enabled || settings.worker.enabled {
            crate::command::commands::squad::runtime_guard::require_container_tier(&self.engines)?;
        }
        // Prepare every fallible start before changing any existing feature or
        // publishing policy. Disables never wait for in-flight jobs.
        let enabled = [
            (FeatureName::Worker, settings.worker.enabled),
            (FeatureName::Squad, settings.squad.enabled),
            (FeatureName::Engine, settings.engine.enabled),
        ];
        for (feature, on) in enabled {
            if on {
                self.supervisors[&feature].prepare_enable().await?;
            }
        }
        let mut started = Vec::new();
        for (feature, on) in enabled {
            let supervisor = &self.supervisors[&feature];
            if on && !supervisor.is_enabled() {
                self.preparing
                    .write()
                    .unwrap()
                    .insert(feature, supervisor.state());
            }
        }
        for (feature, on) in enabled {
            let supervisor = &self.supervisors[&feature];
            if on && !supervisor.is_enabled() {
                if let Err(error) = supervisor.enable_staged(EnabledBy::Config).await {
                    self.rollback_config_starts(&started).await;
                    return Err(error.into());
                }
                started.push(feature);
            }
        }
        if self.core.shutdown().is_cancelled() {
            self.rollback_config_starts(&started).await;
            return Err(CommandError::Other("daemon is shutting down".into()));
        }
        // Certificate writes follow feature preparation/start staging, so a
        // database refusal cannot replace the certificate on disk underneath
        // the listener that continues using the previous configuration.
        let prepared_tls = network::prepare_tls(
            &self.engines.auth_engine,
            self.daemon_paths(),
            &settings.network,
        );
        let tls = match prepared_tls {
            Ok(tls) => tls,
            Err(error) => {
                self.rollback_config_starts(&started).await;
                return Err(error);
            }
        };
        let previous_network = self.config().daemon().network;
        self.network
            .revoke_browser_grants(network::browser_revocations_between(
                &previous_network,
                &settings.network,
            ));
        self.squad_feature
            .apply_settings(settings.squad.clone())
            .await;
        *self.worker.live_config.write().unwrap() = Some(live.clone());
        *self.config.write().unwrap() = live;
        let desired =
            ListenersDesired::new(&settings.network, tls, Some(&self.network.tx.borrow()));
        self.network.tx.send_replace(desired);
        self.preparing.write().unwrap().clear();
        self.core.executor().set_cap(settings.max_concurrent_jobs);
        (self.apply_worker)(settings.worker.clone());
        self.engine_feature.apply_settings(settings.engine.clone());
        for (f, enabled, share) in [
            (
                FeatureName::Worker,
                settings.worker.enabled,
                settings.worker.max_concurrent_jobs,
            ),
            (
                FeatureName::Squad,
                settings.squad.enabled,
                settings.squad.max_concurrent_jobs,
            ),
            (
                FeatureName::Engine,
                settings.engine.enabled,
                settings.engine.max_concurrent_jobs,
            ),
        ] {
            let supervisor = &self.supervisors[&f];
            if enabled {
                supervisor.enable(EnabledBy::Config, None).await?;
            } else if supervisor.enabled_by() == Some(EnabledBy::Config) {
                supervisor.disable(StopMode::Drain).await;
            }
            if supervisor.is_enabled() {
                self.core.executor().set_share(f, share);
            }
        }
        Ok(())
    }
}
