//! The event-based waiting the shared conformance suite asks the tier for.

use awman::engine::conformance::Waiter;
use awman_test_support::wait_for;
use std::time::Duration;

/// Waits with the support crate's `wait_for` (backed-off polling with a
/// deadline). The suite wants a `bool`, so an expired wait is caught and
/// reported as `false`; the suite then fails the check with its own reason
/// (the wait's diagnostics are printed by the panic hook).
pub struct PollWaiter;

impl Waiter for PollWaiter {
    fn wait_until(&self, what: &str, limit: Duration, condition: &mut dyn FnMut() -> bool) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for(what, limit, || condition().then_some(()))
        }))
        .is_ok()
    }
}
