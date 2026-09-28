use crate::engine::error::EngineError;
use std::collections::BTreeMap;
pub const PROTOCOL: &str = "0.7.2/18/awman-1";
pub const LABEL_AWMAN: &str = "awman";
pub const LABEL_NAME: &str = "awman.name";
pub const LABEL_OWNER: &str = "awman.owner.token";
pub const LABEL_PROTOCOL: &str = "awman.worker.protocol";

pub fn sandbox_name_for(name: &str) -> String {
    format!("aw{}", &crate::data::fs::hash::sha256_hex(name)[..14])
}
pub fn labels(
    name: &str,
    owner: &str,
    supplied: &[(String, String)],
) -> Result<BTreeMap<String, String>, EngineError> {
    let mut labels: BTreeMap<_, _> = supplied.iter().cloned().collect();
    for key in [
        LABEL_AWMAN,
        LABEL_NAME,
        LABEL_OWNER,
        LABEL_PROTOCOL,
        "awman.owner.pid",
        "awman.version",
    ] {
        if labels.contains_key(key) {
            return Err(EngineError::Config(format!(
                "reserved builtin label: {key}"
            )));
        }
    }
    for (key, value) in [
        (LABEL_AWMAN, "true".into()),
        (LABEL_NAME, name.into()),
        (LABEL_OWNER, owner.into()),
        (LABEL_PROTOCOL, PROTOCOL.into()),
        ("awman.owner.pid", std::process::id().to_string()),
        ("awman.version", env!("CARGO_PKG_VERSION").into()),
    ] {
        labels.insert(key.into(), value);
    }
    Ok(labels)
}
pub fn check_protocol(labels: &BTreeMap<String, String>) -> Result<(), EngineError> {
    let found = labels
        .get(LABEL_PROTOCOL)
        .map(String::as_str)
        .unwrap_or("unknown");
    if found != PROTOCOL {
        return Err(EngineError::WorkerProtocolMismatch {
            expected: PROTOCOL.into(),
            found: found.into(),
        });
    }
    Ok(())
}
pub fn check_owner(labels: &BTreeMap<String, String>, owner: &str) -> Result<(), EngineError> {
    check_protocol(labels)?;
    if labels.get(LABEL_OWNER).map(String::as_str) != Some(owner) {
        return Err(EngineError::Other(
            "builtin sandbox belongs to another session".into(),
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_fixed_and_labels_cannot_spoof_ownership() {
        let name = sandbox_name_for(&"é/../".repeat(80));
        assert_eq!(name.len(), 16);
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(labels("name", "owner", &[(LABEL_OWNER.into(), "other".into())]).is_err());
        assert!(check_protocol(&BTreeMap::new()).is_err());
    }
    #[test]
    fn reused_pid_does_not_authorize_a_new_owner() {
        let labels = labels("agent", "original-token", &[]).unwrap();
        assert!(check_owner(&labels, "original-token").is_ok());
        assert!(check_owner(&labels, "another-token").is_err());
        let mut old = labels;
        old.insert(LABEL_PROTOCOL.into(), "old-runtime".into());
        assert!(matches!(
            check_owner(&old, "original-token"),
            Err(EngineError::WorkerProtocolMismatch { .. })
        ));
    }
}

#[cfg(test)]
mod hardening_tests;
