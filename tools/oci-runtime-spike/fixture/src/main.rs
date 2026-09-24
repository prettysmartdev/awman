use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;
use tar::{Archive, Builder, EntryType, Header};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn append(builder: &mut Builder<Vec<u8>>, name: &str, data: &[u8], mode: u32) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, name, data)?;
    Ok(())
}

fn directory(builder: &mut Builder<Vec<u8>>, name: &str, mode: u32) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_size(0);
    header.set_mode(mode);
    header.set_uid(1234);
    header.set_gid(1234);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, name, &[][..])?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: awman-oci-spike-fixture BASE_DOCKER_TAR OUTPUT_DIRECTORY".into());
    }
    let mut files = BTreeMap::new();
    let mut archive = Archive::new(fs::File::open(&args[1])?);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() {
            let name = entry
                .path()?
                .to_string_lossy()
                .trim_start_matches("./")
                .to_string();
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            files.insert(name, data);
        }
    }
    let manifest: Value = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or("missing Docker manifest")?,
    )?;
    let image = manifest
        .as_array()
        .and_then(|images| images.first())
        .ok_or("empty Docker archive")?;
    let config_name = image["Config"].as_str().ok_or("missing config")?;
    let mut config: Value =
        serde_json::from_slice(files.get(config_name).ok_or("missing config data")?)?;
    let mut layers = Vec::new();
    for name in image["Layers"].as_array().ok_or("missing layers")? {
        layers.push(
            files
                .get(name.as_str().ok_or("invalid layer name")?)
                .ok_or("missing layer")?
                .clone(),
        );
    }
    let diff_ids = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or("missing diff_ids")?;
    if diff_ids.len() != layers.len() {
        return Err("base layer count does not match config".into());
    }
    for (layer, expected) in layers.iter().zip(diff_ids) {
        if expected.as_str() != Some(format!("sha256:{}", digest(layer)).as_str()) {
            return Err("base must contain uncompressed layers with matching diff_ids".into());
        }
    }

    let mut lower = Builder::new(Vec::new());
    append(&mut lower, "compat/remove-me", b"removed", 0o644)?;
    append(&mut lower, "compat/opaque/old", b"hidden", 0o644)?;
    let lower = lower.into_inner()?;
    let mut upper = Builder::new(Vec::new());
    append(&mut upper, "compat/.wh.remove-me", b"", 0o644)?;
    append(&mut upper, "compat/opaque/.wh..wh..opq", b"", 0o644)?;
    append(&mut upper, "compat/opaque/new", b"visible", 0o644)?;
    append(&mut upper, "compat/mode", b"mode", 0o755)?;
    directory(&mut upper, "home/probe", 0o755)?;
    directory(&mut upper, "image-work", 0o755)?;
    append(
        &mut upper,
        "etc/passwd",
        b"root:x:0:0:root:/root:/bin/sh\nprobe:x:1234:1234:probe:/home/probe:/bin/sh\n",
        0o644,
    )?;
    append(
        &mut upper,
        "etc/group",
        b"root:x:0:\nprobe:x:1234:\n",
        0o644,
    )?;
    append(
        &mut upper,
        "compat/entrypoint",
        b"#!/bin/sh\nprintf 'entrypoint-marker\\n'\nexec \"$@\"\n",
        0o755,
    )?;
    append(
        &mut upper,
        "compat/check-image",
        include_bytes!("../../image-checks.sh"),
        0o755,
    )?;
    let upper = upper.into_inner()?;
    for layer in [&lower, &upper] {
        config["rootfs"]["diff_ids"]
            .as_array_mut()
            .ok_or("missing diff_ids")?
            .push(json!(format!("sha256:{}", digest(layer))));
    }
    layers.extend([lower, upper]);
    config["config"] = json!({
        "User": "probe", "WorkingDir": "/image-work",
        "Env": ["HOME=/home/probe", "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "IMAGE_ENV=image value"],
        "Entrypoint": ["/compat/entrypoint"], "Cmd": ["/compat/check-image"],
        "Labels": {"awman.spike": "fixture"}
    });
    if let Some(history) = config["history"].as_array_mut() {
        history.extend([
            json!({"created_by": "awman spike lower"}),
            json!({"created_by": "awman spike upper"}),
        ]);
    }
    let output = Path::new(&args[2]);
    fs::create_dir_all(output)?;
    let config_bytes = serde_json::to_vec(&config)?;
    let config_hash = digest(&config_bytes);
    let mut docker = Builder::new(Vec::new());
    append(
        &mut docker,
        &format!("{config_hash}.json"),
        &config_bytes,
        0o644,
    )?;
    let layer_names: Vec<_> = layers
        .iter()
        .enumerate()
        .map(|(index, _)| format!("layer-{index}/layer.tar"))
        .collect();
    for (name, layer) in layer_names.iter().zip(&layers) {
        append(&mut docker, name, layer, 0o644)?;
    }
    let docker_manifest = json!([{"Config": format!("{config_hash}.json"), "RepoTags": ["awman-spike/fixture:latest"], "Layers": layer_names}]);
    append(
        &mut docker,
        "manifest.json",
        &serde_json::to_vec(&docker_manifest)?,
        0o644,
    )?;
    fs::write(output.join("fixture-docker.tar"), docker.into_inner()?)?;

    let descriptors: Vec<_> = layers.iter().map(|layer| json!({"mediaType":"application/vnd.oci.image.layer.v1.tar", "digest":format!("sha256:{}", digest(layer)), "size":layer.len()})).collect();
    let oci_manifest = json!({"schemaVersion":2, "mediaType":"application/vnd.oci.image.manifest.v1+json", "config":{"mediaType":"application/vnd.oci.image.config.v1+json", "digest":format!("sha256:{config_hash}"), "size":config_bytes.len()}, "layers":descriptors});
    let manifest_bytes = serde_json::to_vec(&oci_manifest)?;
    let manifest_hash = digest(&manifest_bytes);
    let index = json!({"schemaVersion":2, "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json", "digest":format!("sha256:{manifest_hash}"), "size":manifest_bytes.len(), "platform":{"os":"linux", "architecture":config["architecture"]}, "annotations":{"org.opencontainers.image.ref.name":"awman-spike/fixture:latest"}}]});
    let mut oci = Builder::new(Vec::new());
    append(
        &mut oci,
        "oci-layout",
        b"{\"imageLayoutVersion\":\"1.0.0\"}",
        0o644,
    )?;
    append(&mut oci, "index.json", &serde_json::to_vec(&index)?, 0o644)?;
    append(
        &mut oci,
        &format!("blobs/sha256/{manifest_hash}"),
        &manifest_bytes,
        0o644,
    )?;
    append(
        &mut oci,
        &format!("blobs/sha256/{config_hash}"),
        &config_bytes,
        0o644,
    )?;
    for layer in layers {
        append(
            &mut oci,
            &format!("blobs/sha256/{}", digest(&layer)),
            &layer,
            0o644,
        )?;
    }
    let oci_bytes = oci.into_inner()?;
    let mut check = Archive::new(Cursor::new(&oci_bytes));
    for entry in check.entries()? {
        entry?;
    }
    fs::write(output.join("fixture-oci.tar"), oci_bytes)?;
    fs::write(
        output.join("expected-config.json"),
        serde_json::to_vec_pretty(&config)?,
    )?;
    println!(
        "fixture architecture={} layers={}",
        config["architecture"],
        layer_names.len()
    );
    Ok(())
}
