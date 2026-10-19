//! Safe transport seam; routing and version policy belong to the caller.
use std::collections::BTreeMap;
use std::time::Instant;

/// A value in an XPC dictionary, as far as this contract uses them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XpcValue {
    String(String),
    Data(Vec<u8>),
}

/// The type a reply key is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XpcKind {
    String,
    Data,
}

/// The reply keys that were present with the requested type.
pub type XpcReply = BTreeMap<String, XpcValue>;

/// When a request must stop waiting.
pub struct WaitControl<'a> {
    pub deadline: Instant,
    /// Polled while the request is outstanding; `true` abandons it.
    pub stop: &'a (dyn Fn() -> bool + Sync),
}

/// Why a request produced no reply dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XpcFailure {
    /// The mach service is not registered (the service is not running).
    Unavailable,
    /// The peer went away mid-request (crash or restart).
    Interrupted,
    /// `stop` returned true; the connection was cancelled.
    Stopped,
    /// The deadline passed; the connection was cancelled.
    TimedOut,
    /// The reply was neither a dictionary nor a connection error.
    UnexpectedReply,
}

/// Sends one request dictionary to a mach service and returns the reply.
/// Implemented by the native macOS shim; adapted to the caller's engine trait
/// by the binary and faked in tests.
pub trait XpcTransport: Send + Sync {
    fn send(
        &self,
        service: &str,
        request: &[(&str, XpcValue)],
        reply_keys: &[(&str, XpcKind)],
        wait: &WaitControl<'_>,
    ) -> Result<XpcReply, XpcFailure>;
}
