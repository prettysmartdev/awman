mod lifecycle;

mod api;
mod automation_routes;
mod browser;
mod browser_reauth;
mod git_results;
mod init_api;
mod read_only_status;
mod reload;
mod session_git_auth;
pub(crate) mod support;
// Local transport permissions are applicable on Unix; Windows owner-DACL
// coverage is a native-platform matrix obligation.
#[cfg(unix)]
mod local_security;

mod contexts;

pub(crate) mod lease_exec;
mod workflow_context;

mod credential_updates;

mod process_config;

mod store_unification;

mod session_lifecycle;
