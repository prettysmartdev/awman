//! Untrusted-input soak: random corruption of a valid OCI image archive
//! must be accepted or rejected, never crash the validator. Inputs found by
//! a failing run are printed with their seed and must be added as a
//! regression case to the `unit` tier next to the validator.

use crate::support;
use awman::data::oci_identity::OciPlatform;
use awman::engine::oci::{AcquireLimits, ArchiveValidator};
use awman_test_support::fixtures::OciImageBuilder;
use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, TestRunner};

/// What one corruption does to the archive bytes.
#[derive(Debug, Clone)]
enum Corruption {
    FlipByte { at: usize, mask: u8 },
    Truncate { keep: usize },
    Overwrite { at: usize, bytes: Vec<u8> },
}

fn corruption() -> impl Strategy<Value = Vec<Corruption>> {
    let one = prop_oneof![
        (any::<usize>(), 1u8..=255).prop_map(|(at, mask)| Corruption::FlipByte { at, mask }),
        any::<usize>().prop_map(|keep| Corruption::Truncate { keep }),
        (
            any::<usize>(),
            proptest::collection::vec(any::<u8>(), 1..64)
        )
            .prop_map(|(at, bytes)| Corruption::Overwrite { at, bytes }),
    ];
    proptest::collection::vec(one, 1..4)
}

fn apply(valid: &[u8], steps: &[Corruption]) -> Vec<u8> {
    let mut bytes = valid.to_vec();
    for step in steps {
        if bytes.is_empty() {
            break;
        }
        let len = bytes.len();
        match step {
            Corruption::FlipByte { at, mask } => bytes[at % len] ^= mask,
            Corruption::Truncate { keep } => bytes.truncate(keep % len),
            Corruption::Overwrite { at, bytes: patch } => {
                let start = at % len;
                let end = (start + patch.len()).min(len);
                bytes[start..end].copy_from_slice(&patch[..end - start]);
            }
        }
    }
    bytes
}

#[test]
fn a_corrupted_image_archive_is_accepted_or_rejected_but_never_crashes_the_validator() {
    let sandbox = support::soak_sandbox(
        "soak::fuzz::a_corrupted_image_archive_is_accepted_or_rejected_but_never_crashes_the_validator",
    );
    let image = OciImageBuilder::new()
        .gzip(true)
        .layer(&[("etc/hello", b"hello\n"), ("bin/tool", b"#!/bin/sh\n")])
        .build();
    let valid_path = image.write_archive(&sandbox.tmp().join("valid.tar"));
    let valid = std::fs::read(&valid_path).expect("valid archive");
    let platform = OciPlatform::host_linux();
    let limits = AcquireLimits::DEFAULT;
    let validator = ArchiveValidator {
        platform: &platform,
        wanted_refs: &[],
        limits: &limits,
    };
    validator
        .validate(&valid_path, &mut |_| {})
        .expect("the uncorrupted archive validates");

    let attempt = sandbox.tmp().join("attempt.tar");
    let mut runner = TestRunner::new(Config {
        cases: 2000,
        failure_persistence: Some(Box::new(FileFailurePersistence::Off)),
        ..Config::default()
    });
    let outcome = runner.run(&corruption(), |steps| {
        std::fs::write(&attempt, apply(&valid, &steps)).expect("write corrupted archive");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validator.validate(&attempt, &mut |_| {}).map(|_| ())
        }));
        prop_assert!(result.is_ok(), "the validator panicked on {steps:?}");
        Ok(())
    });
    if let Err(failure) = outcome {
        panic!("{failure}");
    }
    sandbox.finish();
}
