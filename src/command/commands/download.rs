//! `DownloadCommand` — download static assets (per-agent Dockerfile, aspec
//! tarball) into the current repo.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::engine::message::{MessageLevel, UserMessage, UserMessageSink};

/// Typed enum of every asset the `download` command knows how to fetch.
/// Catalogue parsing maps the user-supplied string into this enum so unknown
/// assets fail with a structured `CommandError::Other` rather than a silent
/// 0-byte success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadAsset {
    AspecTarball,
    AgentDockerfile { agent: String },
}

impl DownloadAsset {
    /// Parse a user-supplied asset string. Accepts `aspec` / `aspec-tarball`
    /// for the aspec tarball, and `dockerfile-<agent>` for an agent
    /// Dockerfile. Returns `None` for unknown values; callers translate that
    /// into a structured error.
    pub fn parse(asset: &str) -> Option<Self> {
        if asset == "aspec" || asset == "aspec-tarball" {
            Some(Self::AspecTarball)
        } else if let Some(agent) = asset.strip_prefix("dockerfile-") {
            if agent.is_empty() {
                None
            } else {
                Some(Self::AgentDockerfile {
                    agent: agent.to_string(),
                })
            }
        } else {
            None
        }
    }

    pub fn as_label(&self) -> String {
        match self {
            DownloadAsset::AspecTarball => "aspec".to_string(),
            DownloadAsset::AgentDockerfile { agent } => format!("dockerfile-{agent}"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadOutcome {
    pub asset: String,
    pub bytes_written: usize,
    pub dest_path: Option<String>,
}

pub trait DownloadCommandFrontend: UserMessageSink + Send + Sync {}

pub struct DownloadCommand {
    asset: String,
    engines: Engines,
    session: crate::data::session::Session,
}

impl DownloadCommand {
    pub fn new(asset: String, engines: Engines, session: crate::data::session::Session) -> Self {
        Self {
            asset,
            engines,
            session,
        }
    }
}

#[async_trait]
impl Command for DownloadCommand {
    type Frontend = Box<dyn DownloadCommandFrontend>;
    type Outcome = DownloadOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("download: fetching asset '{}'…", self.asset),
        });
        let parsed = match DownloadAsset::parse(&self.asset) {
            Some(p) => p,
            None => {
                let err = CommandError::Other(format!(
                    "unknown download asset '{}'; expected 'aspec' or 'dockerfile-<agent>'",
                    self.asset
                ));
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("download: unknown asset '{}': {err}", self.asset),
                });
                return Err(err);
            }
        };
        let outcome = match parsed {
            DownloadAsset::AspecTarball => {
                let dest = RepoDockerfilePaths::new(self.session.git_root()).aspec_root();
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "download: fetching aspec tarball…".into(),
                });
                let bytes = match crate::data::network::download_aspec_tarball().await {
                    Ok(b) => b,
                    Err(e) => {
                        let err = CommandError::Other(e.to_string());
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("download: failed to fetch aspec tarball: {e}"),
                        });
                        return Err(err);
                    }
                };
                let bytes_written = bytes.len();
                if let Err(e) = crate::data::network::extract_aspec_tarball(&bytes, &dest) {
                    let err = CommandError::Other(e.to_string());
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("download: failed to extract aspec tarball: {e}"),
                    });
                    return Err(err);
                }
                DownloadOutcome {
                    asset: self.asset,
                    bytes_written,
                    dest_path: Some(dest.display().to_string()),
                }
            }
            DownloadAsset::AgentDockerfile { agent } => {
                let dest =
                    RepoDockerfilePaths::new(self.session.git_root()).agent_dockerfile(&agent);
                let project_tag =
                    crate::data::image_tags::project_image_tag(self.session.git_root());
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!("download: fetching agent image for '{agent}'…"),
                });
                if let Err(e) = crate::engine::agent::download::download_agent_dockerfile(
                    &agent,
                    &dest,
                    &project_tag,
                )
                .await
                {
                    let err = CommandError::Other(e.to_string());
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("download: failed to download agent dockerfile: {e}"),
                    });
                    return Err(err);
                }
                let bytes_written = std::fs::metadata(&dest)
                    .map(|m| m.len() as usize)
                    .unwrap_or(0);
                DownloadOutcome {
                    asset: self.asset,
                    bytes_written,
                    dest_path: Some(dest.display().to_string()),
                }
            }
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "download: complete".into(),
        });
        frontend.replay_queued();
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognises_aspec_aliases() {
        assert_eq!(
            DownloadAsset::parse("aspec"),
            Some(DownloadAsset::AspecTarball)
        );
        assert_eq!(
            DownloadAsset::parse("aspec-tarball"),
            Some(DownloadAsset::AspecTarball)
        );
    }

    #[test]
    fn parse_recognises_agent_dockerfile() {
        let parsed = DownloadAsset::parse("dockerfile-claude");
        assert_eq!(
            parsed,
            Some(DownloadAsset::AgentDockerfile {
                agent: "claude".into()
            })
        );
    }

    #[test]
    fn parse_rejects_empty_dockerfile_agent_name() {
        assert_eq!(DownloadAsset::parse("dockerfile-"), None);
    }

    #[test]
    fn parse_rejects_unknown_asset() {
        assert_eq!(DownloadAsset::parse("nope"), None);
        assert_eq!(DownloadAsset::parse(""), None);
    }
}
