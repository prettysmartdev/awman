use super::schema::*;
pub fn render(g: &GateEvidence, records: &[Evidence]) -> String {
    let mut s=format!("Gate {}: {:?}\n\nQueue: {} ms; execution: {} ms. Budget: {:?} minutes.\n\n| Cell | Outcome | Expected | Ran | Failed | Skipped | Blocked |\n| --- | --- | --- | --- | --- | --- | --- |\n",g.gate,g.verdict,g.queue_ms,g.execution_ms,g.p95_budget_minutes);
    for c in &g.cells {
        s.push_str(&format!(
            "| {} | {:?} | {} | {} | {} | {} | {} |\n",
            c.cell,
            c.outcome,
            c.counts.expected,
            c.counts.ran,
            c.counts.failed,
            c.counts.skipped,
            c.counts.blocked
        ));
    }
    for e in g.errors.iter().chain(&g.missing) {
        s.push_str(&format!("\n- {e}"));
    }
    for e in records {
        s.push_str(&format!(
            "\n\n{}: reproduce `{}`; evidence {}. Cache: {:?}.\n",
            e.cell, e.reproduce, e.run.url, e.cache
        ));
    }
    let mut slow: Vec<_> = records.iter().flat_map(|e| &e.results).collect();
    slow.sort_by_key(|r| std::cmp::Reverse(r.duration_ms));
    s.push_str("\nSlowest tests:\n");
    for r in slow.into_iter().take(10) {
        s.push_str(&format!("- {}: {} ms\n", r.test, r.duration_ms));
    }
    s.push_str(&format!(
        "\nQuarantine: {}\nOverrides: {}\n",
        g.quarantine.join(", "),
        g.overrides_applied.join(", ")
    ));
    s
}
