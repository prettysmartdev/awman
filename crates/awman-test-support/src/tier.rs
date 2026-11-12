//! Test tiers and explicit per-test identity.
//!
//! A test's tier is stated by the test (through [`TestCase`]), never inferred
//! from the executable name, a thread name or global state (design 16.7).

use std::fmt;
use std::time::Duration;

/// The tiers of WI 0144 section 3.3. Each has a time budget per test and a set
/// of guaranteed capabilities, both read from the gate manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// Memory, injected clock/env and a private temp dir. No processes, no sockets.
    Unit,
    /// Loopback, owned subprocesses, local git, the debug `awman` binary, fakes.
    Component,
    /// Real Docker Engine, registries, git transports, Apple Containers, sbx.
    Boundary,
    /// A real VM on native hardware.
    Guest,
    /// The exact release executable under a cleared environment.
    Artifact,
    /// Public network, pressure, long runs. Budget declared per suite.
    Soak,
}

impl Tier {
    /// Every tier, in manifest order.
    pub const ALL: [Tier; 6] = [
        Tier::Unit,
        Tier::Component,
        Tier::Boundary,
        Tier::Guest,
        Tier::Artifact,
        Tier::Soak,
    ];

    /// The manifest key (`[tiers.<name>]`) and the `AWMAN_TEST_TIER` value.
    pub fn name(self) -> &'static str {
        match self {
            Tier::Unit => "unit",
            Tier::Component => "component",
            Tier::Boundary => "boundary",
            Tier::Guest => "guest",
            Tier::Artifact => "artifact",
            Tier::Soak => "soak",
        }
    }

    /// Parse a manifest tier name.
    pub fn from_name(name: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The per-test budget (`budget_seconds`), used as the default process
    /// deadline. Soak has no default; the supervisor then uses one hour and
    /// soak suites set their own deadlines.
    pub fn budget(self) -> Duration {
        crate::config::run_config()
            .tier(self)
            .budget_seconds
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(3600))
    }

    /// Whether tests of this tier may start processes and open sockets.
    pub fn may_spawn(self) -> bool {
        self != Tier::Unit
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Explicit, immutable identity of one test: its ID (module path inside the
/// test binary, e.g. `engine::git::clone_over_ssh`) and its tier.
///
/// ```
/// use awman_test_support::{TestCase, Tier};
/// const CASE: TestCase = TestCase { id: "engine::git::clone_over_ssh", tier: Tier::Component };
/// assert_eq!(CASE.tier, Tier::Component);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestCase {
    /// Module path of the test function inside its binary.
    pub id: &'static str,
    /// The tier the test belongs to.
    pub tier: Tier,
}

impl TestCase {
    /// A `component` case.
    pub const fn component(id: &'static str) -> TestCase {
        TestCase {
            id,
            tier: Tier::Component,
        }
    }

    /// A `boundary` case.
    pub const fn boundary(id: &'static str) -> TestCase {
        TestCase {
            id,
            tier: Tier::Boundary,
        }
    }

    /// A `guest` case.
    pub const fn guest(id: &'static str) -> TestCase {
        TestCase {
            id,
            tier: Tier::Guest,
        }
    }

    /// An `artifact` case.
    pub const fn artifact(id: &'static str) -> TestCase {
        TestCase {
            id,
            tier: Tier::Artifact,
        }
    }

    /// A `soak` case (soak tests may live in a mixed binary; they say so here).
    pub const fn soak(id: &'static str) -> TestCase {
        TestCase {
            id,
            tier: Tier::Soak,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for tier in Tier::ALL {
            assert_eq!(Tier::from_name(tier.name()), Some(tier));
        }
        assert_eq!(Tier::from_name("e2e"), None);
    }

    #[test]
    fn only_unit_may_not_spawn() {
        assert!(!Tier::Unit.may_spawn());
        assert!(Tier::Component.may_spawn());
        assert!(Tier::Guest.may_spawn());
    }

    #[test]
    fn budgets_follow_the_manifest() {
        assert_eq!(Tier::Unit.budget(), Duration::from_secs(1));
        assert_eq!(Tier::Component.budget(), Duration::from_secs(20));
        assert_eq!(Tier::Boundary.budget(), Duration::from_secs(120));
        assert_eq!(Tier::Guest.budget(), Duration::from_secs(300));
        assert_eq!(Tier::Artifact.budget(), Duration::from_secs(300));
    }

    #[test]
    fn constructors_set_the_tier() {
        assert_eq!(TestCase::component("a").tier, Tier::Component);
        assert_eq!(TestCase::boundary("a").tier, Tier::Boundary);
        assert_eq!(TestCase::guest("a").tier, Tier::Guest);
        assert_eq!(TestCase::artifact("a").tier, Tier::Artifact);
        assert_eq!(TestCase::soak("a").tier, Tier::Soak);
    }
}
