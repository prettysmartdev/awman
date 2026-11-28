//! Shared reservation protocol used by the artifact test and the standalone kit.
//! No product library dependency: all mutation is by an exact reserved reference.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub schema: u32,
    pub runtime: String,
    pub owner: String,
    pub project: String,
    pub image_tag: String,
    pub label: String,
    pub root: PathBuf,
    pub deadline: String,
    pub cli_path: PathBuf,
    pub cli_sha256: String,
    pub engine: Option<String>,
    /// Set only after the artifact test proves that every reserved object is absent.
    #[serde(default)]
    pub armed: bool,
}
impl Reservation {
    pub fn read(out: &Path, tmproot: &Path) -> Result<Self> {
        let r: Self = serde_json::from_slice(&std::fs::read(out.join("runtime-owned.json"))?)?;
        let token = r
            .owner
            .strip_prefix("awr-")
            .context("invalid runtime owner")?;
        ensure!(
            r.schema == 1
                && ["apple", "docker"].contains(&r.runtime.as_str())
                && token.len() == 16
                && token
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                && r.project == format!("awsmoke-{token}")
                && r.image_tag == format!("awman-smoke/guest:{}", r.owner)
                && r.label == format!("awman.release.smoke.owner={}", r.owner)
                && r.root == tmproot.canonicalize()?,
            "invalid runtime reservation"
        );
        ensure!(
            r.cli_path.is_absolute()
                && r.cli_sha256.len() == 64
                && r.cli_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid reserved service identity"
        );
        ensure!(
            if r.runtime == "docker" {
                r.engine
                    .as_deref()
                    .is_some_and(|s| !s.is_empty() && !s.contains([',', '\n', '\r']))
            } else {
                r.engine.is_none()
            },
            "invalid reserved runtime endpoint"
        );
        chrono::DateTime::parse_from_rfc3339(&r.deadline).context("invalid runtime deadline")?;
        Ok(r)
    }
    pub fn write(&self, out: &Path) -> Result<()> {
        let tmp = out.join("runtime-owned.json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut f = options.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self)?)?;
        f.sync_all()?;
        std::fs::rename(tmp, out.join("runtime-owned.json"))?;
        Ok(())
    }
    pub fn images(&self) -> [String; 3] {
        [
            format!("awman-{}-claude:latest", self.project),
            format!("awman-{}:latest", self.project),
            self.image_tag.clone(),
        ]
    }
    pub fn owns_image(&self, image: &str) -> bool {
        self.images().iter().any(|s| s == image)
    }
}

/// Implementations must enforce process deadlines and return only successful output.
pub trait Service {
    fn output(&self, args: &[&str]) -> Result<Vec<u8>>;
}
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub containers: Vec<String>,
    pub images: Vec<String>,
}
impl Inventory {
    pub fn len(&self) -> usize {
        self.containers.len() + self.images.len()
    }
}
fn rows(bytes: &[u8], apple: bool) -> Result<Vec<Value>> {
    if apple {
        return serde_json::from_slice(bytes).context("unreadable Apple inventory");
    }
    std::str::from_utf8(bytes)?
        .lines()
        .filter(|s| !s.trim().is_empty())
        .map(|s| serde_json::from_str(s).context("unreadable Docker inventory"))
        .collect()
}
fn field<'a>(v: &'a Value, pointers: &[&str]) -> Result<&'a str> {
    pointers
        .iter()
        .find_map(|p| v.pointer(p).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .context("unverifiable runtime inventory row")
}
pub fn inventory(r: &Reservation, cli: &impl Service) -> Result<Inventory> {
    let apple = r.runtime == "apple";
    let mut result = Inventory::default();
    let containers = cli.output(if apple {
        &["list", "--all", "--format", "json"]
    } else {
        &["ps", "--all", "--no-trunc", "--format", "{{json .}}"]
    })?;
    for v in rows(&containers, apple)? {
        let name = if apple {
            field(&v, &["/configuration/id", "/id", "/name"])?
        } else {
            field(&v, &["/ID"])?
        };
        let image = if apple {
            field(&v, &[
            "/configuration/image/descriptor/annotations/com.apple.containerization.image.name",
            "/configuration/image/reference", "/configuration/image", "/image", "/Image"])?
        } else {
            field(&v, &["/Image"])?
        };
        // Containers use randomized production names. Their exact reserved image
        // reference is the ownership link; an unrelated awman container is untouched.
        let labelled = v
            .get("Labels")
            .and_then(Value::as_str)
            .is_some_and(|s| s.split(',').any(|label| label == r.label))
            || v.pointer("/configuration/labels/awman.release.smoke.owner")
                .and_then(Value::as_str)
                == Some(r.owner.as_str());
        if r.owns_image(image) || labelled {
            ensure!(
                !name.starts_with('-')
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid owned container identifier"
            );
            result.containers.push(name.to_owned());
        }
    }
    let images = cli.output(if apple {
        &["image", "list", "--format", "json"]
    } else {
        &["image", "ls", "--no-trunc", "--format", "{{json .}}"]
    })?;
    for v in rows(&images, apple)? {
        let tag = if apple {
            field(&v, &["/configuration/name", "/reference", "/name"])?.to_owned()
        } else {
            format!("{}:{}", field(&v, &["/Repository"])?, field(&v, &["/Tag"])?)
        };
        if r.owns_image(&tag) {
            result.images.push(tag);
        }
    }
    result.containers.sort();
    result.containers.dedup();
    result.images.sort();
    result.images.dedup();
    Ok(result)
}
/// A nonzero leak count voids a test result even if reconciliation removed it.
/// An unreadable listing never establishes absence.
pub fn reconcile(r: &Reservation, cli: &impl Service, out: &Path) -> Result<bool> {
    let before = inventory(r, cli)?;
    if !r.armed {
        // These names were never claimed. A collision blocks the test but
        // cannot authorize deleting an existing object or count it as a leak.
        std::fs::write(
            out.join("runtime-reconcile.json"),
            serde_json::to_vec_pretty(&json!({
                "schema":1,"leaked":0,"removed":[],"remaining":0,"occupied":before.len(),
                "owner":r.owner,"armed":false,"verified":true
            }))?,
        )?;
        return Ok(false);
    }
    let mut removed = Vec::new();
    let mut removal_errors = Vec::new();
    if r.armed {
        for name in &before.containers {
            // Deletion status alone is insufficient: verify the listing afterwards.
            let verb = if r.runtime == "apple" { "delete" } else { "rm" };
            if let Err(error) = cli.output(&[verb, "--force", name]) {
                removal_errors.push(format!("{verb} {name}: {error:#}"));
            }
            removed.push(name.clone());
        }
        // Remove derived images first, then the pinned base's private tag.
        for tag in r.images() {
            if before.images.contains(&tag) {
                let verb = if r.runtime == "apple" { "delete" } else { "rm" };
                if let Err(error) = cli.output(&["image", verb, &tag]) {
                    removal_errors.push(format!("image {verb} {tag}: {error:#}"));
                }
                removed.push(tag);
            }
        }
    }
    let after = inventory(r, cli)?;
    std::fs::write(
        out.join("runtime-reconcile.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema":1,"leaked":before.len(),"removed":removed,"remaining":after.len(),
            "owner":r.owner,"verified":after.len()==0 && removal_errors.is_empty(),
            "removal_errors": removal_errors
        }))?,
    )?;
    ensure!(after.len() == 0, "owned runtime objects survived removal");
    ensure!(
        removal_errors.is_empty(),
        "runtime removal command failed: {}",
        removal_errors.join("; ")
    );
    Ok(before.len() != 0)
}

/// Adapt a pinned OCI fixture for a private import without changing any blob.
/// Docker gets Docker archive metadata referencing the same uncompressed layers;
/// Apple gets the OCI index with only its display reference changed.
pub fn private_archive(source: &Path, output: &Path, tag: &str, docker: bool) -> Result<()> {
    use std::io::Read;
    let mut writer = tar::Builder::new(std::fs::File::create(output)?);
    let mut indexes = 0;
    let mut docker_manifest = None;
    for entry in tar::Archive::new(std::fs::File::open(source)?).entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let mut header = entry.header().clone();
        if path == Path::new("index.json") {
            indexes += 1;
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            let mut index: Value = serde_json::from_slice(&bytes)?;
            ensure!(
                index["manifests"].as_array().is_some_and(|v| v.len() == 1),
                "fixture must have one OCI manifest"
            );
            ensure!(
                index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"]
                    == "awman-test/guest:fixture",
                "unexpected fixture reference"
            );
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"] = tag.into();
            let bytes = serde_json::to_vec(&index)?;
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            if !docker {
                writer.append_data(&mut header, path, bytes.as_slice())?;
            }
        } else if docker && path == Path::new("oci-layout") {
            // Docker archive metadata below references the same pinned blobs.
        } else if docker && entry.size() < 1024 * 1024 && path.starts_with("blobs/sha256") {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(layers) = v["layers"].as_array() {
                    ensure!(
                        docker_manifest.is_none(),
                        "fixture contains multiple manifests"
                    );
                    let blob = |v: &Value| -> Result<String> {
                        let digest = v["digest"]
                            .as_str()
                            .and_then(|s| s.strip_prefix("sha256:"))
                            .context("invalid OCI descriptor digest")?;
                        ensure!(
                            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                            "invalid OCI blob path"
                        );
                        Ok(format!("blobs/sha256/{digest}"))
                    };
                    let layers: Vec<_> = layers
                        .iter()
                        .map(|v| {
                            ensure!(
                                v["mediaType"] == "application/vnd.oci.image.layer.v1.tar",
                                "pinned fixture layer must be uncompressed"
                            );
                            blob(v)
                        })
                        .collect::<Result<_>>()?;
                    docker_manifest = Some(
                        json!([{"Config":blob(&v["config"])?,"RepoTags":[tag],"Layers":layers}]),
                    );
                }
            }
            writer.append_data(&mut header, path, bytes.as_slice())?;
        } else {
            writer.append_data(&mut header, path, &mut entry)?;
        }
    }
    ensure!(indexes == 1, "fixture must contain one index");
    if docker {
        let bytes = serde_json::to_vec(&docker_manifest.context("missing pinned OCI manifest")?)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        writer.append_data(&mut header, "manifest.json", bytes.as_slice())?;
    }
    writer.finish()?;
    Ok(())
}

/// Version probes are read-only. A client-only answer cannot prove a server.
pub fn versions(runtime: &str, cli: &impl Service) -> Result<(String, String)> {
    let version = String::from_utf8(cli.output(&["--version"])?)?
        .trim()
        .to_owned();
    ensure!(!version.is_empty(), "empty service CLI version");
    let server = if runtime == "apple" {
        cli.output(&["system", "status"])?;
        let value: Value =
            serde_json::from_slice(&cli.output(&["system", "version", "--format", "json"])?)?;
        // Apple 0.12 emits server.version; newer CLIs emit component rows.
        // A client-only answer is never a service version.
        value
            .pointer("/server/version")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .as_array()?
                    .iter()
                    .find(|v| v["appName"] == "container-apiserver")
                    .and_then(|v| v["version"].as_str())
            })
            .filter(|s| !s.is_empty())
            .context("Apple API server version missing")?
            .to_owned()
    } else {
        String::from_utf8(cli.output(&["version", "--format", "{{.Server.Version}}"])?)?
            .trim()
            .to_owned()
    };
    ensure!(
        !server.is_empty(),
        "service did not report a running server"
    );
    Ok((version, server))
}
