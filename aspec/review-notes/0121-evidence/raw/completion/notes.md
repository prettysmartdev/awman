# Completion follow-up — 2026-09-28

User requested closure of every remaining issue. No supplied connection paths
or runner/service credentials have arrived. Git metadata remains absent, as do
KVM, native Apple/x86 hosts, real Docker/Apple services and signing. Do not mark
WI 0119 or WI 0121 complete. Mandatory missing implementations are still FAIL,
not converted to BLOCKED merely because a native host is absent.

Implemented locally:
- Atomic archive lease return and staging lease; retained through backend
  projection, SDK import and identity publication; cloned return keeps lease.
- Authoritative acquisition environment snapshots; explicit production capture
  of source settings and named auth variables. Credentials/proxy captured once
  after cache miss, retained across retry, absent inputs ignored on cache hits.
- Deadline starts before cache lookup; bounded lock polling; checked local copy,
  validation/decompression reads, hash reads and final reference publication;
  success after expiry/cancellation is rejected. Filesystem calls remain
  cooperative, not preemptible. Full phase-specific slow/fault coverage and
  production/header cancellation remain open.
- Hostname allow rules limited to TCP; direct pinned SDK evaluator test exercises
  cached hostname binding denial for UDP/443, permits TCP and allowed DNS and
  rejects unlisted DNS and unrelated TCP addresses. Strict allowed HTTPS remains
  broken; no vendor network semantic relaxation was attempted.
- Shared production API/squad HTTPS setup chooses ring per server, preserves
  h2/http/1.1 ALPN, validates PEM and key/cert pairing without exposing secrets.
  Real trusted/untrusted TLS handshake test, including builtin dependency graph.

Initial compile/test iterations caught and fixed typed acquisition test helpers,
large enum variant, unused test import and ambiguous `.into()` inference.
Intermediate failures remain in the logs. `pre-push-final.log` exits 0 but
predates the final ALPN/deadline adjustment; `run-gates.sh` records the reviewed
stable-source invocations, exits and enforced prerequisite failures.
