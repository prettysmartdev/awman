//! WI 0142 acceptance gate; real production routes over an owner-only Unix socket.
#![cfg(unix)]
use awman::{command, data, engine, frontend};
#[path = "../helpers/api_process.rs"]
mod api_process;
mod cli_bytes;
mod latency;
mod multi_frontend;
mod outcomes;
mod security;
#[path = "../helpers/engine_daemon.rs"]
mod support;
mod terminal;
mod web_frontend;
mod web_matrix;
