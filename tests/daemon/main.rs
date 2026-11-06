//! Process-level tests of the unified `awman daemon` (WI 0140).
//!
//! Every test boots the shipped binary (`awman daemon start`) in a throwaway
//! home with a fake `docker` on its PATH (`tests/helpers/api_process.rs`) and
//! talks to it the way a user does: HTTP over the owner-only local socket
//! (through the loopback proxy `ApiServer` provides) and `awman` subprocesses
//! (`config set`, `daemon api`, `daemon stop`).
//!
//! concurrency.rs            — squad + worker on one database, no mutual exclusion
//! feature_switching.rs      — `config set daemon.features.*`, drain, cancel
//! ephemeral_engine.rs       — ephemeral engine enablement and idle-out
//! network_listener.rs       — network listener, TLS, live exposure and revocation
//! local_listener_security.rs— no TCP, private socket, tampered sidecar
//! status_shape.rs           — `GET /v1/status` naming and round trip
//! keys.rs                   — `/v1/keys` lifecycle, health
//! lifecycle.rs              — start/stop/status/logs and moved-key hints

#![cfg(unix)]

#[path = "../helpers/api_process.rs"]
mod api_process;
mod support;

mod concurrency;
mod ephemeral_engine;
mod feature_switching;
mod keys;
mod lifecycle;
mod local_listener_security;
mod network_listener;
mod status_shape;
