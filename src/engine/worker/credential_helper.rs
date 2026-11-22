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
