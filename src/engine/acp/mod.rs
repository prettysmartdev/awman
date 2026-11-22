//! ACP (Agent Client Protocol) support.
//!
//! This is a Layer 1 module.  It owns the JSON-RPC transport and the portable
//! session driver; command and frontend layers only supply launch plumbing and
//! an [`AcpFrontend`]. Every queue between the agent's pipes and the driver is
//! bounded ([`budget`]).

pub mod budget;
pub mod client;
pub mod frontend;
pub mod protocol;
pub mod session;

#[cfg(test)]
mod bounds_tests;

pub use budget::{AcpBudgets, AcpQueueStats, QueueUsage};
pub use client::{
    AcpCancelHandle, AcpClient, AcpIncoming, AcpTransport, AcpTransportFrontend, AcpUpdates,
    ContainerFileSystem, DenyHostFilesystem, IncomingRequest,
};
pub use frontend::AcpFrontend;
pub use protocol::{
    AvailableCommand, ContentBlock, InitializeRequest, InitializeResponse, JsonRpcId,
    PermissionDecision, PermissionOption, PermissionRequest, PlanEntry, PromptResponse,
    SessionUpdate, ToolCall, ToolCallUpdate,
};
pub use session::{AcpSession, PermissionPolicy};
