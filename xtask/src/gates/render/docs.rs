use crate::manifest::Manifest;
pub fn render(m: &Manifest) -> String {
    let mut text=String::from("<!-- gates:begin contract -->\nGenerated from `ci/gates.toml` (schema 1).\n\n| Gate | Budget (minutes) | Required status |\n| --- | --- | --- |\n");
    for (id, g) in &m.gates {
        text.push_str(&format!(
            "| {id} | {} | {} |\n",
            g.budget_minutes
                .map(|n| n.to_string())
                .unwrap_or_else(|| "inherited".into()),
            g.required_check.as_deref().unwrap_or("gate")
        ));
    }
    text.push_str("\n| Tier | Per-test deadline (seconds) | Guarantees |\n| --- | --- | --- |\n");
    for (id, t) in &m.tiers {
        text.push_str(&format!(
            "| {id} | {} | {} |\n",
            t.budget_seconds
                .map(|n| n.to_string())
                .unwrap_or_else(|| "suite-specific".into()),
            t.guarantees.join(", ")
        ));
    }
    text.push_str("\nUnit, lint, doctest and parity suites always run. Unknown/shared paths select the full PR set. Main and merge-group run the full set. Required skips, missing evidence and expired quarantine fail the gate.\n<!-- gates:end contract -->\n");
    text
}
