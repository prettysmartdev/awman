//! Caller-provided values that live only in daemon memory (WI 0145):
//! per-command `secrets` and per-session Git credentials (`git_auth`).
//! The durable store records names (commands) or the credential kind
//! (sessions) only, so a restart is detected as `secrets_lost` /
//! `credentials_lost` instead of running without the values.
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::data::git_transport::GitAuthSpec;
use crate::data::ids::CommandId;
use crate::data::secret::CommandSecrets;
use crate::engine::git::credential_socket::{CacheSocketListener, CredentialSocketServer};
use crate::engine::git::lease_transport::{CredentialScope, HelperProgram};

/// One push session's credential and the repository it may be offered to.
#[derive(Debug)]
pub struct SessionGitCredential {
    pub spec: Arc<GitAuthSpec>,
    pub scope: CredentialScope,
}

#[derive(Clone, Default)]
pub struct EphemeralSecrets {
    commands: Arc<Mutex<HashMap<CommandId, Arc<CommandSecrets>>>>,
    /// Session id → credential. Inserted at admission, removed on close,
    /// on setup failure and on admission rollback.
    sessions: Arc<Mutex<HashMap<String, Arc<SessionGitCredential>>>>,
    /// The daemon's host-Git credential socket, started on first use.
    socket: Arc<Mutex<Option<Arc<CredentialSocketServer>>>>,
    /// The program Git runs as the session helper. `None`: this executable
    /// (`awman __git-credential-helper`), which is what production uses.
    helper_program: Arc<Mutex<Option<HelperProgram>>>,
    // A scoped engine bundle carries the invocation, never mutates a Session.
    current: Option<Arc<CommandSecrets>>,
    /// The running command's container credential listener (teardown/setup
    /// `push_branch` / `pull_branch` of a `git_auth` session).
    current_git_socket: Option<Arc<CacheSocketListener>>,
}

impl EphemeralSecrets {
    pub fn for_execution(
        &self,
        id: &CommandId,
        names: Option<&[String]>,
    ) -> Result<Option<Arc<CommandSecrets>>, crate::command::error::CommandError> {
        let secrets = self.get(id);
        if names.is_some_and(|names| !names.is_empty()) && secrets.is_none() {
            return Err(crate::command::error::CommandError::CommandSecretsLost);
        }
        Ok(secrets)
    }
    pub fn insert(&self, id: CommandId, secrets: Arc<CommandSecrets>) {
        self.commands
            .lock()
            .expect("command secrets lock")
            .insert(id, secrets);
    }
    pub fn get(&self, id: &CommandId) -> Option<Arc<CommandSecrets>> {
        self.commands
            .lock()
            .expect("command secrets lock")
            .get(id)
            .cloned()
    }
    pub fn remove(&self, id: &CommandId) {
        self.commands
            .lock()
            .expect("command secrets lock")
            .remove(id);
    }
    pub fn scoped(&self, secrets: Option<Arc<CommandSecrets>>) -> Self {
        Self {
            current: secrets,
            ..self.clone()
        }
    }
    pub fn current(&self) -> Option<Arc<CommandSecrets>> {
        self.current.clone()
    }

    // ── Session Git credentials ──────────────────────────────────────────

    pub fn insert_session_git_auth(&self, session_id: &str, credential: SessionGitCredential) {
        self.sessions
            .lock()
            .expect("session credentials lock")
            .insert(session_id.to_string(), Arc::new(credential));
    }
    pub fn session_git_auth(&self, session_id: &str) -> Option<Arc<SessionGitCredential>> {
        self.sessions
            .lock()
            .expect("session credentials lock")
            .get(session_id)
            .cloned()
    }
    pub fn remove_session_git_auth(&self, session_id: &str) {
        self.sessions
            .lock()
            .expect("session credentials lock")
            .remove(session_id);
    }
    pub fn register_session_redaction(
        &self,
        session_id: &str,
        set: &crate::engine::auth::redaction::RedactionSet,
    ) {
        use base64::Engine as _;
        if let Some(credential) = self.session_git_auth(session_id) {
            set.add(credential.spec.secret().clone());
            set.add(crate::data::secret::SecretString::new(
                base64::engine::general_purpose::STANDARD.encode(format!(
                    "{}:{}",
                    credential.spec.username(),
                    credential.spec.secret().expose()
                )),
            ));
        }
    }
    pub fn register_current_git_redaction(
        &self,
        set: &crate::engine::auth::redaction::RedactionSet,
    ) {
        if let Some(socket) = &self.current_git_socket {
            socket.register_redaction(set);
        }
    }

    /// The daemon's host credential socket under `private_dir` (created
    /// owner-only), started on first use and shared by every session.
    pub fn host_socket(
        &self,
        private_dir: &Path,
    ) -> Result<Arc<CredentialSocketServer>, crate::command::error::CommandError> {
        let mut socket = self.socket.lock().expect("credential socket lock");
        if let Some(server) = socket.as_ref() {
            return Ok(Arc::clone(server));
        }
        let configured = self
            .helper_program
            .lock()
            .expect("credential helper program lock")
            .clone();
        let program = match configured {
            Some(program) => program,
            None => crate::command::worker::credential_helper_program()
                .map_err(|e| crate::command::error::CommandError::Other(e.to_string()))?,
        };
        let server = CredentialSocketServer::start(private_dir, program)?;
        *socket = Some(Arc::clone(&server));
        Ok(server)
    }

    /// Run `program` as the session helper instead of this executable. For
    /// hosts that embed the daemon in another binary (the component test
    /// harness, which runs the built `awman`); call before the first session
    /// credential is used.
    pub fn use_helper_program(&self, program: HelperProgram) {
        *self
            .helper_program
            .lock()
            .expect("credential helper program lock") = Some(program);
    }

    /// A view whose container steps may use `listener` (one command).
    pub fn with_container_git_socket(&self, listener: Option<Arc<CacheSocketListener>>) -> Self {
        Self {
            current_git_socket: listener,
            ..self.clone()
        }
    }
    pub fn container_git_socket(&self) -> Option<Arc<CacheSocketListener>> {
        self.current_git_socket.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::secret::SecretString;

    #[test]
    fn fresh_memory_with_a_durable_names_marker_fails_preflight() {
        let id = CommandId::new();
        let names = vec!["TOKEN".into()];
        let memory = EphemeralSecrets::default();
        let values = Arc::new(
            CommandSecrets::new([("TOKEN".into(), SecretString::new("test-secret-lifetime"))])
                .unwrap(),
        );
        memory.insert(id.clone(), values.clone());
        assert!(memory.for_execution(&id, Some(&names)).unwrap().is_some());
        let scoped = memory.scoped(Some(values));
        assert!(scoped.current().is_some());
        memory.remove(&id);
        assert!(scoped.get(&id).is_none());
        let restarted = EphemeralSecrets::default();
        let error = restarted.for_execution(&id, Some(&names)).unwrap_err();
        assert_eq!(
            (error.stage(), error.code(), error.exit_code()),
            ("preflight", "secrets_lost", 1)
        );
        assert!(restarted.for_execution(&id, None).unwrap().is_none());
    }

    #[test]
    fn session_credentials_are_shared_by_scoped_views_and_removed_on_close() {
        use crate::data::git_transport::{GitAuthKind, GitAuthSpec};
        let memory = EphemeralSecrets::default();
        let scoped = memory.scoped(None).with_container_git_socket(None);
        memory.insert_session_git_auth(
            "ses_1",
            SessionGitCredential {
                spec: Arc::new(
                    GitAuthSpec::new(
                        GitAuthKind::Token,
                        None,
                        SecretString::new("test-secret-session"),
                    )
                    .unwrap(),
                ),
                scope: CredentialScope::for_http_url("https://git.example/o/r").unwrap(),
            },
        );
        assert!(scoped.session_git_auth("ses_1").is_some());
        assert!(scoped.session_git_auth("ses_2").is_none());
        scoped.remove_session_git_auth("ses_1");
        assert!(memory.session_git_auth("ses_1").is_none());
        assert!(!format!("{:?}", memory.sessions).contains("test-secret"));
        assert_eq!(
            crate::command::error::CommandError::CredentialsLost.code(),
            "credentials_lost"
        );
    }
}
