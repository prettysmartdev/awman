//! Container naming helpers.
//!
//! `amux-<pid>-<nanos>` for ephemeral runs.

use std::time::{SystemTime, UNIX_EPOCH};

/// Generate an ephemeral container name: `amux-<pid>-<unix-nanos>`.
pub fn generate_container_name() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("amux-{pid}-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_format_starts_with_amux() {
        let n = generate_container_name();
        assert!(n.starts_with("amux-"), "got: {n}");
        assert!(n.len() > "amux-".len() + 5);
    }

    #[test]
    fn names_are_unique_across_calls() {
        let a = generate_container_name();
        std::thread::sleep(std::time::Duration::from_nanos(2));
        let b = generate_container_name();
        assert_ne!(a, b);
    }
}
