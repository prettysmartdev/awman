//! `engine::remote` — HTTP transport and the typed client for an awman
//! daemon.
//!
//! Layer 1 (WI 0114 F-28, decision Q1). `HttpCore` is the tree's single
//! `reqwest::Client` construction; every route-specific client is a thin
//! typed façade over one of them. Living here rather than in Layer 2 is what
//! lets the engine's own HTTP users — `engine::agent::download` and
//! `engine::workflow::poll_ci`'s `CiPoller` — share it.

pub mod client;
pub mod events;
pub mod http_core;

pub use client::{
    ExecArg, ExecJobResponse, JobStatus, RemoteClient, RemoteResponse, SessionSetupStatusResponse,
    StartSessionRequest, StartSessionResponse,
};
pub use events::ExecutionEventSink;
pub use http_core::{HttpClientOptions, HttpCore, HttpResponse};
