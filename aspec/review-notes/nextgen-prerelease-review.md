# nextgen pre-release review

Started 2026-11-20 against `nextgen` at `49eff9f8`. Read-only sweep by eleven
parallel reviewers; each section below is recorded as its reviewer returns.
Findings are the reviewers' own and are marked CONFIRMED or PLAUSIBLE by them;
nothing here has been independently re-verified or fixed unless a line says so.

Scope note: the web app (`web/`) is prerelease and not part of the upcoming
release, so it received only a light pass.

## Status

| # | Area | Status |
|---|---|---|
| 1 | Architecture adherence | pending |
| 2 | Host/container isolation security | pending |
| 3 | API, daemon and network security | pending |
| 4 | Runtime backend parity (builtin vs Docker, in-process vs daemon) | pending |
| 5 | Engine reliability, performance and idioms | pending |
| 6 | Command layer correctness and spec drift | pending |
| 7 | Frontends, mode parity and API/wire consistency | pending |
| 8 | Data layer, file formats, wire formats and layouts | pending |
| 9 | Test flakiness, performance and coverage | pending |
| 10 | Release scripts, CI readiness and repo hygiene | pending |
| 11 | Docs accuracy, spec drift and UX | pending |

## Noticed while setting up (unverified, covered by reviewers 1 and 10)

- `LINUX_BUILTIN_HANDOFF.md` at the repo root is a tracked agent handoff note
  that includes an SSH host address and remote paths.
- `aspec/devops/architecture-lint.md` says the lint is wired into
  `.github/workflows/test.yml`; the workflows are `ci.yml`, `nightly.yml`,
  `candidate.yml`, `promote.yml` and `fixtures.yml`.
