use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};
pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0_u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
/// serde_json's default map is a sorted BTreeMap; conversion also sorts struct keys.
/// Null options remain explicit; array order is significant.
pub fn input_digest<T: Serialize>(input: &T) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&serde_json::to_value(input)?)?))
}
