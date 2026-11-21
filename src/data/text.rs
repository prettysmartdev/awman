//! Pure string helpers shared across layers.
//!
//! Layer 0 because they are arithmetic over text: no filesystem, no process,
//! no policy. Two different "did you mean" implementations had grown — one in
//! the TUI's command box with a distance threshold of 4 and one in
//! `ConfigCommand` with a threshold of 3 — so the same typo could be corrected
//! in a config field name and not in a command name (WI 0114 F-43). There is
//! one implementation now; the *threshold* stays with the caller, because how
//! near a miss is worth offering depends on the vocabulary being searched.

/// Levenshtein edit distance between `a` and `b`, counted in `char`s.
///
/// Counting `char`s rather than bytes matters: a non-ASCII field value or a
/// pasted command with a smart quote would otherwise score as several edits
/// per character.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = b.len();

    // One row at a time: the full matrix is never needed, only the previous
    // row, and a command catalogue is walked once per keystroke in the TUI.
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut current = vec![0usize; n + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            current[j + 1] = if ca == cb {
                prev[j]
            } else {
                1 + prev[j].min(prev[j + 1]).min(current[j])
            };
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[n]
}

/// The candidates within `max_distance` edits of `input`, nearest first.
///
/// Ties keep the order `candidates` were given in, so a caller that passes a
/// deterministic list gets a deterministic answer.
pub fn nearest<'a>(input: &str, candidates: &[&'a str], max_distance: usize) -> Vec<&'a str> {
    let mut scored: Vec<(usize, usize, &'a str)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            let distance = levenshtein(input, candidate);
            (distance <= max_distance).then_some((distance, order, *candidate))
        })
        .collect();
    scored.sort_by_key(|(distance, order, _)| (*distance, *order));
    scored
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}
