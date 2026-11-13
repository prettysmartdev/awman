//! Deterministic guest image recipe. Consumes a digest-verified Docker archive;
//! emits both Docker and OCI layouts with explicit filesystem/identity contracts.
use crate::evidence::digest::sha256;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, io::Read, path::Path};
use tar::{Archive, Builder, Header};

fn append(tar: &mut Builder<Vec<u8>>, name: &str, bytes: &[u8], mode: u32, uid: u64) -> Result<()> {
    let mut h = Header::new_gnu();
    h.set_size(bytes.len() as u64);
    h.set_mode(mode);
    h.set_uid(uid);
    h.set_gid(uid);
    h.set_mtime(0);
    if name.ends_with('/') {
        h.set_entry_type(tar::EntryType::Directory);
    }
    h.set_cksum();
    tar.append_data(&mut h, name, bytes)?;
    Ok(())
}
pub(super) fn build(base: &Path, out: &Path) -> Result<()> {
    let mut files = BTreeMap::new();
    for item in Archive::new(fs::File::open(base)?).entries()? {
        let mut item = item?;
        if !item.header().entry_type().is_file() {
            continue;
        }
        let name = item
            .path()?
            .to_string_lossy()
            .trim_start_matches("./")
            .to_string();
        let mut data = vec![];
        item.read_to_end(&mut data)?;
        ensure!(
            files.insert(name, data).is_none(),
            "duplicate file in base archive"
        );
    }
    let get = |name: &str| {
        files
            .get(name)
            .with_context(|| format!("missing base member {name}"))
    };
    let manifest: Value = serde_json::from_slice(get("manifest.json")?)?;
    let images = manifest.as_array().context("Docker image list")?;
    ensure!(images.len() == 1, "base must contain one image");
    let mut config: Value =
        serde_json::from_slice(get(images[0]["Config"].as_str().context("config path")?)?)?;
    ensure!(
        config["os"] == "linux" && config["architecture"] == "arm64",
        "native fixture requires linux/arm64 base"
    );
    let mut layers = vec![];
    for name in images[0]["Layers"].as_array().context("layers")? {
        layers.push(get(name.as_str().context("layer path")?)?.clone());
    }
    let ids = config["rootfs"]["diff_ids"]
        .as_array()
        .context("diff IDs")?;
    ensure!(
        ids.len() == layers.len(),
        "base layer count differs from config"
    );
    for (bytes, id) in layers.iter().zip(ids) {
        ensure!(
            *id == format!("sha256:{}", sha256(bytes)),
            "base layer is compressed or has the wrong digest"
        );
    }
    let mut lower = Builder::new(vec![]);
    append(&mut lower, "compat/remove-me", b"lower", 0o644, 0)?;
    append(&mut lower, "compat/opaque/old", b"lower", 0o644, 0)?;
    let mut upper = Builder::new(vec![]);
    for (path, bytes, mode) in [
        ("compat/.wh.remove-me", &b""[..], 0o644),
        ("compat/opaque/.wh..wh..opq", &b""[..], 0o644),
        ("compat/opaque/new", &b"visible"[..], 0o644),
        ("compat/mode", &b"mode"[..], 0o755),
        (
            "etc/passwd",
            &b"root:x:0:0:root:/root:/bin/sh\nprobe:x:1234:1234:probe:/home/probe:/bin/sh\n"[..],
            0o644,
        ),
        ("etc/group", &b"root:x:0:\nprobe:x:1234:\n"[..], 0o644),
        (
            "compat/entrypoint",
            &b"#!/bin/sh\nprintf 'entrypoint-marker\\n'\nexec \"$@\"\n"[..],
            0o755,
        ),
        (
            "compat/check-image",
            include_bytes!("../../../../tests/guest/support/image-contract.sh").as_slice(),
            0o755,
        ),
    ] {
        append(&mut upper, path, bytes, mode, 0)?;
    }
    for dir in ["home/probe/", "image-work/"] {
        append(&mut upper, dir, b"", 0o755, 1234)?;
    }
    for layer in [lower.into_inner()?, upper.into_inner()?] {
        config["rootfs"]["diff_ids"]
            .as_array_mut()
            .unwrap()
            .push(json!(format!("sha256:{}", sha256(&layer))));
        layers.push(layer);
    }
    config["config"] = json!({"User":"probe","WorkingDir":"/image-work","Env":["HOME=/home/probe","PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin","IMAGE_ENV=image value"],"Entrypoint":["/compat/entrypoint"],"Cmd":["/compat/check-image"],"Labels":{"org.awman.fixture":"guest-contract"}});
    if let Some(history) = config["history"].as_array_mut() {
        history.extend([
            json!({"created_by":"guest contract lower layer"}),
            json!({"created_by":"guest contract upper layer"}),
        ]);
    }
    let config = serde_json::to_vec(&config)?;
    let config_hash = sha256(&config);
    let mut docker = Builder::new(vec![]);
    let mut oci = Builder::new(vec![]);
    append(
        &mut docker,
        &format!("{config_hash}.json"),
        &config,
        0o644,
        0,
    )?;
    append(
        &mut oci,
        &format!("blobs/sha256/{config_hash}"),
        &config,
        0o644,
        0,
    )?;
    let mut names = vec![];
    let mut descriptors = vec![];
    for (index, layer) in layers.iter().enumerate() {
        let hash = sha256(layer);
        let name = format!("layer-{index}/layer.tar");
        append(&mut docker, &name, layer, 0o644, 0)?;
        append(&mut oci, &format!("blobs/sha256/{hash}"), layer, 0o644, 0)?;
        names.push(name);
        descriptors.push(json!({"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":format!("sha256:{hash}"),"size":layer.len()}));
    }
    let tag = "awman-test/guest:fixture";
    append(
        &mut docker,
        "manifest.json",
        &serde_json::to_vec(
            &json!([{"Config":format!("{config_hash}.json"),"RepoTags":[tag],"Layers":names}]),
        )?,
        0o644,
        0,
    )?;
    let manifest = serde_json::to_vec(
        &json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{config_hash}"),"size":config.len()},"layers":descriptors}),
    )?;
    let hash = sha256(&manifest);
    append(
        &mut oci,
        &format!("blobs/sha256/{hash}"),
        &manifest,
        0o644,
        0,
    )?;
    append(
        &mut oci,
        "oci-layout",
        b"{\"imageLayoutVersion\":\"1.0.0\"}",
        0o644,
        0,
    )?;
    append(
        &mut oci,
        "index.json",
        &serde_json::to_vec(
            &json!({"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":format!("sha256:{hash}"),"size":manifest.len(),"platform":{"os":"linux","architecture":"arm64"},"annotations":{"org.opencontainers.image.ref.name":tag}}]}),
        )?,
        0o644,
        0,
    )?;
    fs::create_dir_all(out)?;
    fs::write(out.join("fixture-docker.tar"), docker.into_inner()?)?;
    fs::write(out.join("fixture-oci.tar"), oci.into_inner()?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base(path: &Path, valid: bool) {
        let layer = Builder::new(Vec::new()).into_inner().unwrap();
        let hash = if valid {
            sha256(&layer)
        } else {
            "0".repeat(64)
        };
        let config = json!({"os":"linux","architecture":"arm64","rootfs":{"type":"layers","diff_ids":[format!("sha256:{hash}")]},"config":{}});
        let mut tar = Builder::new(Vec::new());
        append(
            &mut tar,
            "config.json",
            &serde_json::to_vec(&config).unwrap(),
            0o644,
            0,
        )
        .unwrap();
        append(&mut tar, "layer.tar", &layer, 0o644, 0).unwrap();
        append(
            &mut tar,
            "manifest.json",
            br#"[{"Config":"config.json","Layers":["layer.tar"]}]"#,
            0o644,
            0,
        )
        .unwrap();
        fs::write(path, tar.into_inner().unwrap()).unwrap();
    }
    #[test]
    fn repeatable_image_contains_digest_verified_identity_and_whiteout_layers() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("base.tar");
        base(&input, true);
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        build(&input, &first).unwrap();
        build(&input, &second).unwrap();
        for name in ["fixture-docker.tar", "fixture-oci.tar"] {
            assert_eq!(
                fs::read(first.join(name)).unwrap(),
                fs::read(second.join(name)).unwrap()
            );
        }
        let mut files = BTreeMap::new();
        for e in Archive::new(fs::File::open(first.join("fixture-oci.tar")).unwrap())
            .entries()
            .unwrap()
        {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().to_string();
            let mut bytes = Vec::new();
            e.read_to_end(&mut bytes).unwrap();
            if let Some(hash) = name.strip_prefix("blobs/sha256/") {
                assert_eq!(hash, sha256(&bytes));
            }
            files.insert(name, bytes);
        }
        let index: Value = serde_json::from_slice(&files["index.json"]).unwrap();
        let read = |d: &Value| {
            files[&format!(
                "blobs/sha256/{}",
                d["digest"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("sha256:")
                    .unwrap()
            )]
                .clone()
        };
        let manifest: Value = serde_json::from_slice(&read(&index["manifests"][0])).unwrap();
        let config: Value = serde_json::from_slice(&read(&manifest["config"])).unwrap();
        assert_eq!(config["config"]["User"], "probe");
        assert_eq!(config["config"]["WorkingDir"], "/image-work");
        assert_eq!(manifest["layers"].as_array().unwrap().len(), 3);
        let upper = read(&manifest["layers"][2]);
        let names: Vec<_> = Archive::new(upper.as_slice())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().into_owned())
            .collect();
        assert!(names.contains(&"compat/.wh.remove-me".into()));
        assert!(names.contains(&"compat/opaque/.wh..wh..opq".into()));
        assert!(names.contains(&"compat/check-image".into()));
    }
    #[test]
    fn corrupt_base_cannot_create_a_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("base.tar");
        base(&input, false);
        let out = dir.path().join("out");
        assert!(build(&input, &out)
            .unwrap_err()
            .to_string()
            .contains("wrong digest"));
        assert!(!out.exists());
    }
}
