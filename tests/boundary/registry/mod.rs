//! Boundary tests: image acquisition from a real OCI registry on loopback.
//!
//! The registry is the run's `local-registry` capability (a registry
//! container the job started on the granted Docker Engine). Both
//! capabilities are required: `BLOCKED[docker]` / `BLOCKED[local-registry]`
//! when the run does not name them.

mod client;
mod proxy;
mod world;

use crate::support;
use awman::engine::conformance::registry::oci_registry;
use awman_test_support::capability::LocalRegistry;

/// awman's image acquirer pulling from a real registry conforms: a pull by
/// tag yields the published manifest and config identity, cached-only
/// acquisition makes no request, a missing tag or another platform is an
/// error that caches nothing, and the layer limit refuses before caching.
#[test]
fn a_real_registry_conforms_to_the_acquirer_contract() {
    let sandbox = support::sandbox("registry::a_real_registry_conforms_to_the_acquirer_contract");
    let registry = sandbox.require::<LocalRegistry>();
    let addr = registry.addr;
    let world = world::RealRegistryWorld::new(&sandbox, addr);
    let report = oci_registry(&world);
    report.assert_conforms();
    assert!(
        report.not_applicable.is_empty(),
        "a real registry declares every check applicable: {:?}",
        report.not_applicable
    );
    drop(world);
    sandbox.finish();
}
