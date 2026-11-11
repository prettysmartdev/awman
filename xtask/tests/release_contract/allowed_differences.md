The only normalized difference in the fresh-run transcript is remote tag timing:

- Push: `Pushed tag v1.2.3` becomes `Started the release candidate for v1.2.3; the tag is pushed when it is published`.
- After the watched chain, `Tag v1.2.3 already on origin` reports promotion's tag.
- Unmanaged runs say `Candidate started` in place of `Tag pushed`; the follow
  command names `candidate.yml`. These describe the same tag-timing change.

Steps, prompts, defaults, aborts, other results and scratch repository/release end
state are compared exactly. The unmanaged follow line names candidate.yml, as in
design §12.3. Source-bound resume text replaces the obsolete sentinel text; a
skip never prints that tests passed. Existing candidates add the WI-required
resume report. The legacy fallback is retained unchanged until P4/P5.

The comparison covers success, off-main/divergence/dirty defaults, notes decline,
test skip, `none`, and `auto` with an online runner. For unmanaged runs it also
executes the fake automatic workflow before comparing the eventual end state.

The harness first reads `e4cae907:scripts/release.sh` using git show. If the Git
object database is unavailable it explicitly identifies its comparison as a
retained-baseline comparison. That result cannot close the P4 history-provenance
condition. Re-run `bash xtask/tests/release_contract/compare-transcripts.sh OUT`
from a full checkout on GNU/Linux and macOS before cutover.
