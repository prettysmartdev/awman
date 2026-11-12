mod lifecycle;

mod api;
mod browser;
mod browser_reauth;
mod reload;
mod support;
// Local transport permissions are applicable on Unix; Windows owner-DACL
// coverage is a native-platform matrix obligation.
#[cfg(unix)]
mod local_security;
