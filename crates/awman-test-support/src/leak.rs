//! Resources a test left behind.

use serde::Serialize;
use std::fmt;

/// One leaked resource found by the end-of-test leak check
/// ([`crate::TestSandbox::finish`]) or reported by an `on_finish` probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Leak {
    /// `process`, `port`, `socket`, `vm`, `image`, `container` or `tempdir`.
    pub kind: String,
    /// What exactly was left (label, address, path, identifier).
    pub detail: String,
}

impl Leak {
    /// A leak of `kind` with `detail`.
    pub fn new(kind: &str, detail: impl Into<String>) -> Leak {
        Leak {
            kind: kind.to_string(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Leak {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_kind_and_detail() {
        assert_eq!(
            Leak::new("port", "127.0.0.1:0").to_string(),
            "port: 127.0.0.1:0"
        );
    }
}
