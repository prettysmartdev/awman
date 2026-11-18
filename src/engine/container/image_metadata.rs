//! Local, bounded image queries for CLI container backends.
use super::{process::ContainerCli, runtime::ImageMetadata};
use crate::data::image_inputs::{RepoKey, REPO_LABEL};
use crate::engine::error::EngineError;
use serde_json::Value;

fn invalid() -> EngineError {
    EngineError::Container("unreadable image metadata".into())
}

pub(crate) fn parse_inspection(
    bytes: &[u8],
    tag: &str,
    apple: bool,
) -> Result<ImageMetadata, EngineError> {
    let rows: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let row = rows.get(0).ok_or_else(invalid)?;
    let (id, config, size) = if apple {
        let variants = row
            .get("variants")
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        let platform = crate::data::oci_identity::OciPlatform::host_linux();
        let variant = variants
            .iter()
            .find(|v| {
                v.pointer("/platform/architecture")
                    .or_else(|| v.pointer("/config/architecture"))
                    .and_then(Value::as_str)
                    == Some(platform.architecture.as_str())
            })
            .or_else(|| (variants.len() == 1).then(|| &variants[0]))
            .ok_or_else(invalid)?;
        let id = variant.get("digest");
        let size = variant
            .get("size")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                variant
                    .pointer("/manifest/layers")
                    .and_then(Value::as_array)
                    .map(|layers| {
                        layers
                            .iter()
                            .filter_map(|l| l.get("size").and_then(Value::as_u64))
                            .sum()
                    })
                    .unwrap_or(0)
            });
        (id, variant.pointer("/config/config"), size)
    } else {
        (
            row.get("Id"),
            row.get("Config"),
            row.get("Size").and_then(Value::as_u64).unwrap_or(0),
        )
    };
    let id = id
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)?
        .to_owned();
    let labels = match config.and_then(|v| v.get("Labels")) {
        None | Some(Value::Null) => Default::default(),
        Some(value) => serde_json::from_value(value.clone()).map_err(|_| invalid())?,
    };
    let tags = row
        .get("RepoTags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| vec![tag.into()]);
    Ok(ImageMetadata {
        id,
        labels,
        tags,
        size_bytes: size,
    })
}

pub(super) fn inspect(cli: ContainerCli, tag: &str) -> Result<Option<ImageMetadata>, EngineError> {
    let output = match crate::engine::host_cli::checked_output(
        cli.command().args(["image", "inspect", tag]),
        cli.bin,
    ) {
        Ok(output) => output,
        Err(EngineError::Container(detail))
            if {
                let error = detail.to_ascii_lowercase();
                error.contains("no such image") || error.contains("image not found")
            } =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    parse_inspection(&output.stdout, tag, cli.bin == "container").map(Some)
}

pub(super) fn list_by_repo(
    cli: ContainerCli,
    repo: &RepoKey,
) -> Result<Vec<ImageMetadata>, EngineError> {
    let mut command = cli.command();
    if cli.bin == "container" {
        command.args(["image", "list", "--format", "json"]);
    } else {
        command.args([
            "image",
            "ls",
            "--no-trunc",
            "--filter",
            &format!("label={REPO_LABEL}={}", repo.as_str()),
            "--format",
            "{{.ID}}",
        ]);
    }
    let output = crate::engine::host_cli::checked_output(&mut command, cli.bin)?;
    if !output.status.success() {
        return Err(EngineError::Container(format!(
            "{} image listing failed",
            cli.bin
        )));
    }
    let references: Vec<String> = if cli.bin == "container" {
        let rows: Vec<Value> = serde_json::from_slice(&output.stdout).map_err(|_| invalid())?;
        rows.iter()
            .filter_map(|row| {
                row.pointer("/configuration/name")
                    .or_else(|| row.get("reference"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    } else {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut images: std::collections::BTreeMap<String, ImageMetadata> =
        std::collections::BTreeMap::new();
    for reference in references {
        if let Some(image) = inspect(cli.clone(), &reference)? {
            if image.labels.get(REPO_LABEL).map(String::as_str) == Some(repo.as_str()) {
                if let Some(existing) = images.get_mut(&image.id) {
                    for tag in image.tags {
                        if !existing.tags.contains(&tag) {
                            existing.tags.push(tag);
                        }
                    }
                } else {
                    images.insert(image.id.clone(), image);
                }
            }
        }
    }
    Ok(images.into_values().collect())
}

pub(super) fn remove_apple(cli: ContainerCli, reference: &str) -> Result<(), EngineError> {
    let mut references = vec![reference.to_string()];
    // Apple identifies variants by manifest digest, but deletion resolves
    // store references. Translate a variant ID back to its installed tags.
    if reference.starts_with("sha256:") {
        let output = crate::engine::host_cli::checked_output(
            cli.command().args(["image", "list", "--format", "json"]),
            cli.bin,
        )?;
        if !output.status.success() {
            return Err(EngineError::Container(
                "Apple image listing failed before removal".into(),
            ));
        }
        let rows: Vec<Value> = serde_json::from_slice(&output.stdout).map_err(|_| invalid())?;
        let matches: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.pointer("/configuration/descriptor/digest")
                    .or_else(|| row.pointer("/descriptor/digest"))
                    .and_then(Value::as_str)
                    == Some(reference)
                    || row
                        .get("variants")
                        .and_then(Value::as_array)
                        .is_some_and(|variants| {
                            variants.iter().any(|variant| {
                                variant.get("digest").and_then(Value::as_str) == Some(reference)
                            })
                        })
            })
            .filter_map(|row| {
                row.pointer("/configuration/name")
                    .or_else(|| row.get("reference"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        if !matches.is_empty() {
            references = matches;
        }
    }
    for reference in references {
        let output = crate::engine::host_cli::checked_output(
            cli.command().args(["image", "rm", &reference]),
            cli.bin,
        )?;
        if !output.status.success() {
            return Err(EngineError::Container("Apple image removal failed".into()));
        }
    }
    Ok(())
}
