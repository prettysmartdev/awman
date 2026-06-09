//! Docker Sandbox driver (`sbx` CLI).
//!
//! Stubbed in WI 0089 — every `SandboxBackend` method returns
//! `EngineError::NotImplemented`. WI 0090 lands the real implementation.

mod backend;

pub(super) use backend::DSbxBackend;
