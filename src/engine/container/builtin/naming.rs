use crate::engine::error::EngineError;
use std::collections::BTreeMap;
pub const PROTOCOL: &str = crate::data::builtin_runtime::WORKER_PROTOCOL;
pub const LABEL_AWMAN: &str = "awman";
pub const LABEL_NAME: &str = "awman.name";
pub const LABEL_OWNER: &str = "awman.owner.token";
pub const LABEL_PROTOCOL: &str = "awman.worker.protocol";
pub const LABEL_OWNER_PID: &str = "awman.owner.pid";
pub const LABEL_VERSION: &str = "awman.version";
pub const LABEL_VCPUS: &str = "awman.vcpus";
pub const LABEL_MEMORY_MIB: &str = "awman.memory_mib";
pub const LABEL_NETWORK_MODE: &str = "awman.network.mode";

/// Sizing recorded on the VM at launch so `inspect` can read it back.
#[derive(Debug, Clone, Copy)]
pub struct LaunchLabels<'a> {
    pub vcpus: u8,
    pub memory_mib: u32,
    pub network_mode: &'a str,
}

pub fn sandbox_name_for(name: &str) -> String {
    format!("aw{}", &crate::data::fs::hash::sha256_hex(name)[..14])
}
pub fn labels(
    name: &str,
    owner: &str,
    launch: LaunchLabels<'_>,
    supplied: &[(String, String)],
) -> Result<BTreeMap<String, String>, EngineError> {
    let mut labels: BTreeMap<_, _> = supplied.iter().cloned().collect();
    for key in [
        LABEL_AWMAN,
        LABEL_NAME,
        LABEL_OWNER,
        LABEL_PROTOCOL,
        LABEL_OWNER_PID,
        LABEL_VERSION,
        LABEL_VCPUS,
        LABEL_MEMORY_MIB,
        LABEL_NETWORK_MODE,
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
        (LABEL_OWNER_PID, std::process::id().to_string()),
        (LABEL_VERSION, env!("CARGO_PKG_VERSION").into()),
        (LABEL_VCPUS, launch.vcpus.to_string()),
        (LABEL_MEMORY_MIB, launch.memory_mib.to_string()),
        (LABEL_NETWORK_MODE, launch.network_mode.into()),
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
