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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_is_zero_for_equal_strings() {
        assert_eq!(levenshtein("chat", "chat"), 0);
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn distance_counts_each_edit_once() {
        assert_eq!(levenshtein("cht", "chat"), 1, "one insertion");
        assert_eq!(levenshtein("chats", "chat"), 1, "one deletion");
        assert_eq!(levenshtein("chit", "chat"), 1, "one substitution");
        assert_eq!(levenshtein("", "chat"), 4);
    }

    /// The byte-wise implementations this replaces scored a two-byte character
    /// as two edits.
    #[test]
    fn distance_counts_characters_not_bytes() {
        assert_eq!(levenshtein("café", "cafe"), 1);
    }

    #[test]
    fn nearest_orders_by_distance_then_by_input_order() {
        let candidates = ["chat", "clean", "config"];
        assert_eq!(nearest("cht", &candidates, 3), vec!["chat"]);
        assert_eq!(
            nearest("cha", &candidates, 4),
            vec!["chat", "clean"],
            "a wider threshold admits the further candidate, still nearest first"
        );
        assert_eq!(nearest("zzzzzzzz", &candidates, 3), Vec::<&str>::new());
    }

    #[test]
    fn nearest_respects_the_callers_threshold() {
        let candidates = ["chat"];
        assert!(nearest("cht", &candidates, 0).is_empty());
        assert_eq!(nearest("cht", &candidates, 1), vec!["chat"]);
    }
}
