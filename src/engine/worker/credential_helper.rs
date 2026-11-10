//! Lease repository credentials: installing the one-shot Git credential
//! helper for a lease and refreshing it from the work server.
//!
//! The helper itself (owner-only file, nonce-only argv, reset of inherited
//! helpers, no global/system Git config) lives in
//! [`crate::engine::git::lease_transport`] so every lease Git operation is
//! built by the same transport builder.

use std::sync::Arc;

use async_trait::async_trait;

use super::error::WorkerEngineError;
use super::lease_workspace::LeaseWorkspace;
use super::push::CredentialSource;
use super::session::{LeaseApi, LeaseRef};
pub use crate::engine::git::lease_transport::{
    answer_credential_request, HelperProgram, LeaseCredentialHelper, LeaseCredentials,
    LeaseTransport, LeaseUrlPolicy, CREDENTIAL_HELPER_SUBCOMMAND, LEASE_CREDENTIAL_FILE_ENV,
};

/// The lease transport for `item`: its credentials (if any) installed in the
/// workspace's private directory, which is never mounted into the run.
/// Unsupported credentials fail closed (the caller releases `ineligible`).
pub fn lease_transport(
    workspace: &LeaseWorkspace,
    item: &awman_wsp::WorkItem,
    program: HelperProgram,
    policy: LeaseUrlPolicy,
) -> Result<LeaseTransport, WorkerEngineError> {
    let creds = LeaseCredentials::from_wsp(item.repo.credentials.as_ref())
        .map_err(|e| WorkerEngineError::Invalid(e.to_string()))?;
    let transport = if creds.has_secret() {
        let helper = LeaseCredentialHelper::install(
            workspace.private_dir(),
            &item.repo.url,
            &creds,
            program,
            policy,
        )
        .map_err(|e| WorkerEngineError::Invalid(e.to_string()))?;
        LeaseTransport::with_helper(Arc::new(helper), policy)
    } else {
        LeaseTransport::anonymous(policy)
    };
    transport
        .check_url(&item.repo.url)
        .map_err(|e| WorkerEngineError::Invalid(e.to_string()))?;
    Ok(transport)
}

/// [`CredentialSource`] over `POST /v1/leases/{id}/credentials`.
pub struct WspCredentialSource {
    api: Arc<dyn LeaseApi>,
    lease: LeaseRef,
}

impl WspCredentialSource {
    pub fn new(api: Arc<dyn LeaseApi>, lease: LeaseRef) -> Self {
        Self { api, lease }
    }
}

#[async_trait]
impl CredentialSource for WspCredentialSource {
    async fn fresh(&self) -> Result<LeaseCredentials, WorkerEngineError> {
        let creds = self.api.credentials(&self.lease).await?;
        let creds = LeaseCredentials::from_wsp(Some(&creds))
            .map_err(|e| WorkerEngineError::Invalid(e.to_string()))?;
        if !creds.has_secret() {
            return Err(WorkerEngineError::Invalid(
                "fresh lease credentials carry no secret".into(),
            ));
        }
        Ok(creds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ids::LeaseId;

    const SECRET: &str = "wsp-repo-token-77";

    fn item(kind: awman_wsp::CredentialKind, token: Option<&str>) -> awman_wsp::WorkItem {
        let mut item = awman_wsp::conformance::work_item("01J");
        item.repo.url = "https://git.example/team/app.git".into();
        item.repo.credentials = Some(awman_wsp::Credentials {
            kind,
            token: token.map(Into::into),
            username: None,
            expires_at: None,
        });
        item
    }

    #[test]
    fn helper_reads_secret_from_private_file_not_argv() {
        let root = tempfile::tempdir().unwrap();
        let ws = LeaseWorkspace::prepare(&root.path().join("w"), &LeaseId::new()).unwrap();
        let transport = lease_transport(
            &ws,
            &item(awman_wsp::CredentialKind::Bearer, Some(SECRET)),
            HelperProgram::awman("/usr/local/bin/awman".into()),
            LeaseUrlPolicy::default(),
        )
        .unwrap();
        let helper = transport.helper().expect("installed");
        assert!(helper.file().starts_with(ws.private_dir()));
        assert!(
            !helper.file().starts_with(ws.clone_dir()),
            "never inside the mounted clone"
        );
        assert!(helper.git_config_args().iter().all(|a| !a.contains(SECRET)));
        assert!(helper.env().iter().all(|(_, v)| !v.contains(SECRET)));
        assert!(std::fs::read_to_string(helper.file())
            .unwrap()
            .contains(SECRET));
        // No credentials: anonymous transport, still isolated.
        let mut anon = item(awman_wsp::CredentialKind::None, None);
        assert!(lease_transport(
            &ws,
            &anon,
            HelperProgram::awman("/a".into()),
            LeaseUrlPolicy::default()
        )
        .unwrap()
        .helper()
        .is_none());
        anon.repo.url = "https://user:pw@git.example/r.git".into();
        anon.repo.credentials = None;
        assert!(lease_transport(
            &ws,
            &anon,
            HelperProgram::awman("/a".into()),
            LeaseUrlPolicy::default()
        )
        .is_err());
        // Unsupported credential shapes fail closed.
        assert!(lease_transport(
            &ws,
            &item(awman_wsp::CredentialKind::Basic, Some(SECRET)),
            HelperProgram::awman("/a".into()),
            LeaseUrlPolicy::default()
        )
        .is_err());
    }

    #[test]
    fn debug_and_display_redact_token() {
        let root = tempfile::tempdir().unwrap();
        let ws = LeaseWorkspace::prepare(&root.path().join("w"), &LeaseId::new()).unwrap();
        let work = item(awman_wsp::CredentialKind::Bearer, Some(SECRET));
        assert!(!format!("{:?}", work.repo.credentials).contains(SECRET));
        let transport = lease_transport(
            &ws,
            &work,
            HelperProgram::awman("/a".into()),
            LeaseUrlPolicy::default(),
        )
        .unwrap();
        assert!(!format!("{transport:?}").contains(SECRET));
        let creds = LeaseCredentials::from_wsp(work.repo.credentials.as_ref()).unwrap();
        assert!(!format!("{creds:?}").contains(SECRET));
        let error =
            WorkerEngineError::Invalid("lease credentials are only supported for HTTPS".into());
        assert!(!error.to_string().contains(SECRET));
    }
}
