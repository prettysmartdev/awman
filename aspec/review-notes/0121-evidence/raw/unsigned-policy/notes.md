# Signing/notarization scope removal — 2026-09-28

The user explicitly removed signing and notarization from all requirements.
This supersedes earlier scope and access requests, not historical execution
records. WI 0119–0123 and current register distribution rows now require the
actual final artifact without explicit signing/notarization steps. Runtime and
CI documentation and workflow context give the same instruction.

Removed from active automation: signing runner label/identities/secrets,
Developer ID and installer credential checks, codesign commands/preflight,
notarytool/stapler/spctl, signed package generation/collection, native/test/spike
ad-hoc signing and mandatory signature/entitlement inspection. Renamed the Mac
release job/artifact and its dependency references; release uploads the binary
plus checksum and traces/boots that actual copy. Upstream/toolchain behavior
is not modified. Native boot remains mandatory and no OS permission error may
be converted to a pass. No native Mac execution occurred on this Linux host.

Validation:
- Workflow YAML parsed with the existing serde_yaml_ng dependency; every job
  dependency resolves; every run block passes bash syntax; no explicit signing
  operation remains. Exact checker source and output are preserved here.
- Initial YAML validation caught an existing unquoted colon in a test step
  name; quoting the name fixes its syntax. Retained initial failure log.
- Modified native/spike scripts pass bash -n; cargo fmt --all --check passes.
- Initial make pre-push failed one existing source-contract assertion that
  required com.apple.security.hypervisor in CI; updated its prerequisite list
  to the retained KVM/HVF host checks. Hardware execution is still required.
- Final make pre-push result is recorded in command-exits.tsv/pre-push-final.log.

Git/full-diff identity remains unavailable; fresh present-source hashes are in
source.sha256. Prior closure/raw snapshots keep their original hashes and logs.
No remaining mandatory code, hardware, services or final-artifact evidence was
marked complete by removing signing from scope.
