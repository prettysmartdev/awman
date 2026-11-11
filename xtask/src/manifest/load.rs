use super::Manifest;
use anyhow::{Context, Result};
use std::path::Path;

pub fn load(root: &Path) -> Result<Manifest> {
    parse(&std::fs::read(root.join("ci/gates.toml")).context("reading ci/gates.toml")?)
}
pub fn parse(bytes: &[u8]) -> Result<Manifest> {
    let mut manifest: Manifest = toml::from_str(std::str::from_utf8(bytes)?)
        .context("gate manifest schema 1 (unknown fields are forbidden)")?;
    manifest.version = crate::evidence::digest::sha256(bytes);
    anyhow::ensure!(
        manifest.schema == 1,
        "unsupported gate manifest schema {}",
        manifest.schema
    );
    Ok(manifest)
}
