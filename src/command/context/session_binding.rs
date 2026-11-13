//! Session `context_key`: validation at admission and the binding applied to
//! the live session when it is created or restored after a daemon restart.
//!
//! The key only selects *which* directory `context(repo)` resolves to
//! (`RepoContextScope`); an overlay still decides whether it is mounted.
use crate::{
    command::error::CommandError,
    data::{
        context::ContextKey,
        session::{RepoContextBinding, Session},
    },
};

/// `None` → `Ok(None)`. An invalid key is refused before anything is
/// persisted, as an invalid-request-class error naming the rule.
pub fn plan_key(raw: Option<&str>) -> Result<Option<ContextKey>, CommandError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    ContextKey::parse(raw)
        .map(Some)
        .map_err(|error| CommandError::InvalidArgumentValue {
            command: vec!["worker".into(), "sessions".into()],
            argument: "context_key".into(),
            reason: error.to_string(),
        })
}

/// Apply a stored key to a session being created or restored. A staged
/// (lease) binding is never replaced: a lease must not reach a host key. A
/// stored value that no longer parses (hand-edited row) leaves the session
/// on the inferred directory and is logged, never mounted.
pub fn apply(session: &mut Session, stored: Option<&str>) {
    if matches!(session.repo_context(), RepoContextBinding::Staged(_)) {
        return;
    }
    let binding = match stored.map(ContextKey::parse) {
        None => RepoContextBinding::Inferred,
        Some(Ok(key)) => RepoContextBinding::Keyed(key),
        Some(Err(_)) => {
            tracing::warn!(
                session_id = %session.id().as_uuid(),
                "Stored context_key is invalid; using the inferred repo context"
            );
            RepoContextBinding::Inferred
        }
    };
    session.set_repo_context(binding);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_key_accepts_absent_and_valid_and_refuses_invalid() {
        assert_eq!(plan_key(None).unwrap(), None);
        assert_eq!(
            plan_key(Some("team.memory-1")).unwrap().unwrap().as_str(),
            "team.memory-1"
        );
        for bad in ["", "Upper", "-lead", "../x", "a/b", &"a".repeat(129)] {
            let error = plan_key(Some(bad)).unwrap_err();
            assert!(
                matches!(error, CommandError::InvalidArgumentValue { .. }),
                "{bad}: {error:?}"
            );
        }
    }
}
