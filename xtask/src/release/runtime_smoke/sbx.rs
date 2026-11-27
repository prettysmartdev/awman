//! Release-time SBX smoke (WI 0094): lifecycle + exact-asset readiness on the frozen hosts.
//! Implementation lives with the developer diagnostic it shares code with.
pub(crate) use crate::sbx_validation::release::{
    cli_identity, probe, reconcile, run, select, verify_report, CliIdentity, SbxHostSelection,
    SbxSelection, SbxSmokeBinding, SbxSmokeJob, SbxSmokeRecord, Target, LIMITS, SCHEMA,
};
