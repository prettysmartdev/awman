//! Container naming helpers.
//!
//! `awman-<pid>-<nanos>` for ephemeral runs, and
//! `awman-squad-<task-slug>-<8 hex>` for squad's background agents.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::engine::error::EngineError;

/// Generate an ephemeral container name: `awman-<pid>-<subsec-nanos>`.
pub fn generate_container_name() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("awman-{pid}-{nanos}")
}

// ─── squad container naming ───────────────────────────────────────────────────
//
// squad's background agents are named `awman-squad-<task-slug>-<8 hex>`.
// The container name — not a label — is the single identity channel every
// runtime tier honours (Docker `--filter name=`, Apple client-side prefix,
// sandbox `sbx ls` prefix), so `awman agents status` and name-based discovery derive
// the squad marker from the name, never from a label (Apple cannot read labels
// back).

/// Common prefix on every squad container name.
pub const SQUAD_NAME_PREFIX: &str = "awman-squad-";

/// Maximum task-slug length (keeps the full container name a legal
/// identifier on every backend).
const TASK_SLUG_MAX_LEN: usize = 48;

/// Length of the fixed-width uniqueness token appended to a squad name.
const SQUAD_UNIQUE_HEX_LEN: usize = 8;

/// Generate a squad container name: `awman-squad-<slug>-<8 lowercase hex>`.
///
/// The uniqueness token is the first 8 hex chars of a v4 UUID — random, not
/// time-based, so two tasks ticking in the same instant never collide.
/// Its **fixed** 8-char width is what makes [`parse_squad_task_slug`]
/// unambiguous even when the slug itself contains hyphens.
///
/// The caller is expected to have validated `task_slug` with
/// [`validate_task_slug`] at task-creation time.
pub fn generate_squad_container_name(task_slug: &str) -> String {
    let unique: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(SQUAD_UNIQUE_HEX_LEN)
        .collect();
    format!("{SQUAD_NAME_PREFIX}{task_slug}-{unique}")
}

/// Recover the task slug from a squad container name.
///
/// Strips the `awman-squad-` prefix and a trailing `-[0-9a-f]{8}` token,
/// returning the slug in between (which may itself contain hyphens). Returns
/// `None` for any name that is not a well-formed squad name — including
/// ephemeral `awman-<pid>-<nanos>` names and a name whose slug would be empty.
pub fn parse_squad_task_slug(container_name: &str) -> Option<&str> {
    let rest = container_name.strip_prefix(SQUAD_NAME_PREFIX)?;
    // The trailing token is a hyphen plus exactly 8 hex chars.
    if rest.len() <= SQUAD_UNIQUE_HEX_LEN {
        return None;
    }
    let (slug, tail) = rest.split_at(rest.len() - SQUAD_UNIQUE_HEX_LEN - 1);
    let mut tail_chars = tail.chars();
    if tail_chars.next() != Some('-') {
        return None;
    }
    if !tail_chars.all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) {
        return None;
    }
    // Guard against `awman-squad--<hex>` (empty slug).
    if slug.is_empty() {
        return None;
    }
    Some(slug)
}

/// Validate a task slug against `[a-z0-9]([a-z0-9-]*[a-z0-9])?`,
/// 1..=48 chars. Rejected slugs would produce an illegal container name on
/// one or more backends.
pub fn validate_task_slug(slug: &str) -> Result<(), EngineError> {
    let invalid = |reason: &str| {
        EngineError::Config(format!(
            "invalid task name {slug:?}: {reason}; \
             names must match [a-z0-9]([a-z0-9-]*[a-z0-9])? and be 1..={TASK_SLUG_MAX_LEN} chars"
        ))
    };
    if slug.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if slug.len() > TASK_SLUG_MAX_LEN {
        return Err(invalid("too long"));
    }
    let is_lower_alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !slug.chars().all(|c| is_lower_alnum(c) || c == '-') {
        return Err(invalid(
            "may only contain lowercase letters, digits, and hyphens",
        ));
    }
    // First and last chars must be alphanumeric (no leading/trailing hyphen).
    let first = slug.chars().next().unwrap();
    let last = slug.chars().next_back().unwrap();
    if !is_lower_alnum(first) || !is_lower_alnum(last) {
        return Err(invalid("must start and end with a letter or digit"));
    }
    Ok(())
}
