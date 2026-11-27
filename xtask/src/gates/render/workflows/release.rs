//! Native kits share an asset's hosted build and are independently checkpointed.
use super::pieces::{ATTEST, UPLOAD};
use crate::manifest::Manifest;
use anyhow::Result;
pub fn render(m: &Manifest) -> Result<String> {
    let mut extra = String::new();
    for (id, n) in &m.release.native {
        if !n.required {
            continue;
        }
        // The original workflow preserves its primary (offline boot) kit path.
        let primary = m
            .release
            .native
            .iter()
            .find(|(_, other)| other.required && other.asset == n.asset)
            .map(|(id, _)| id);
        if primary == Some(id) {
            continue;
        }
        let step = id.replace(['.', '-'], "_");
        extra.push_str(&format!(r#"      - name: Checkpoint availability for {id}
        id: {step}
        if: ${{{{ inputs.asset == '{asset}' }}}}
        env:
          GH_TOKEN: ${{{{ github.token }}}}
        run: |
          artifacts=$(gh api "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/artifacts")
          expired=$(printf '%s' "$artifacts" | jq --arg name "native-kit-{id}" '[.artifacts[] | select(.name == $name and .expired == true)] | length')
          count=$(printf '%s' "$artifacts" | jq --arg name "native-kit-{id}" '[.artifacts[] | select(.name == $name and .expired == false)] | length')
          [ "$expired" -eq 0 ] && [ "$count" -le 1 ] || {{ echo 'expired or ambiguous native kit checkpoint' >&2; exit 1; }}
          echo "reuse=$([ "$count" -eq 1 ] && echo true || echo false)" >> "$GITHUB_OUTPUT"
      - name: Native test kit {id}
        if: ${{{{ inputs.asset == '{asset}' && steps.{step}.outputs.reuse != 'true' }}}}
        run: |
          mkdir -p out
          cargo run --locked --quiet -p xtask --target-dir "$XTASK_TARGET" -- test install-runner
          cargo run --locked --quiet -p xtask --target-dir "$XTASK_TARGET" -- test kit --native '{id}' --commit "$COMMIT" --out "$RUNNER_TEMP/kit-{id}"
          cargo run --locked --quiet -p xtask --target-dir "$XTASK_TARGET" -- release kit-pack --dir "$RUNNER_TEMP/kit-{id}" --out 'out/native-kit-{id}.tar'
      - name: Attest native kit {id}
        if: ${{{{ inputs.asset == '{asset}' && steps.{step}.outputs.reuse != 'true' }}}}
        uses: {ATTEST}
        with:
          subject-path: out/native-kit-{id}.tar
      - name: Checkpoint native kit {id}
        if: ${{{{ inputs.asset == '{asset}' && steps.{step}.outputs.reuse != 'true' }}}}
        uses: {UPLOAD}
        with:
          name: native-kit-{id}
          path: out/native-kit-{id}.tar
          if-no-files-found: error
          retention-days: 90
"#, asset=n.asset));
    }
    Ok(include_str!("release.yml").replace("@@EXTRA_NATIVE@@", &extra))
}
