mod lifecycle;

mod api;
mod browser;
mod browser_reauth;
mod git_results;
mod reload;
mod session_git_auth;
mod support;
// Local transport permissions are applicable on Unix; Windows owner-DACL
// coverage is a native-platform matrix obligation.
#[cfg(unix)]
mod local_security;

mod contexts;

mod workflow_context;
