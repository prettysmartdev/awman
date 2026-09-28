//! OCI content identity — platform, digest, and the identity of an acquired
//! image.
//!
//! An image the builtin runtime runs is identified by content (manifest and
//! config digests), by platform, and by the kind of source it came from — not
//! by its name alone. Two images with the same reference from different
//! sources are different identities.

use serde::{Deserialize, Serialize};

use crate::data::config::image_source::ImageSourceKind;

/// An OCI platform (`os`/`architecture`[/`variant`]), in OCI spelling
/// (`amd64`, `arm64`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OciPlatform {
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl OciPlatform {
    /// `linux/<host architecture>` — the only platform a guest on this host
    /// can run. Architecture uses OCI spelling (`x86_64` → `amd64`,
    /// `aarch64` → `arm64`).
    pub fn host_linux() -> Self {
        Self {
            os: "linux".into(),
            architecture: oci_architecture(std::env::consts::ARCH).into(),
            variant: None,
        }
    }

    /// Whether an image built for `other` runs where `self` is wanted.
    ///
    /// OS and architecture must be equal. A missing variant on either side
    /// matches any variant, and `arm64` treats an absent variant as `v8`.
    pub fn matches(&self, other: &Self) -> bool {
        if self.os != other.os || self.architecture != other.architecture {
            return false;
        }
        let normalize = |p: &Self| -> Option<String> {
            match (p.architecture.as_str(), p.variant.as_deref()) {
                ("arm64", None) => Some("v8".into()),
                (_, v) => v.map(str::to_string),
            }
        };
        match (normalize(self), normalize(other)) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

impl std::fmt::Display for OciPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(variant) = &self.variant {
            write!(f, "/{variant}")?;
        }
        Ok(())
    }
}

/// Rust `target_arch` → OCI architecture name.
fn oci_architecture(rust_arch: &str) -> &str {
    match rust_arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

/// A `sha256:<64 lowercase hex>` content digest. Validated on construction
/// and on deserialization, so an unvalidated digest cannot exist.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    /// Parse `sha256:<64 lowercase hex>`. Other algorithms are rejected.
    pub fn parse(s: &str) -> Result<Self, String> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or_else(|| format!("digest `{s}` must start with `sha256:`"))?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!(
                "digest `{s}` must be `sha256:` followed by 64 lowercase hex characters"
            ));
        }
        Ok(Self(s.to_string()))
    }

    /// The full `sha256:<hex>` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Digest {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Digest::parse(&value)
    }
}

impl From<Digest> for String {
    fn from(value: Digest) -> Self {
        value.0
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identity of an image acquired for the builtin runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageIdentity {
    /// The reference as resolved against its source.
    pub reference: String,
    /// Digest of the platform-specific image manifest.
    pub manifest_digest: Digest,
    /// Digest of the image config blob.
    pub config_digest: Digest,
    /// Platform of the manifest that was selected.
    pub platform: OciPlatform,
    /// Where the content came from.
    pub source: ImageSourceKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn digest_accepts_only_sha256_lower_hex() {
        assert!(Digest::parse(&format!("sha256:{HEX}")).is_ok());
        assert!(Digest::parse(HEX).is_err());
        assert!(Digest::parse(&format!("sha512:{HEX}")).is_err());
        assert!(Digest::parse(&format!("sha256:{}", HEX.to_uppercase())).is_err());
        assert!(Digest::parse(&format!("sha256:{}", &HEX[1..])).is_err());
        assert!(Digest::parse("sha256:").is_err());
    }

    #[test]
    fn digest_deserialization_validates() {
        let ok: Digest = serde_json::from_str(&format!("\"sha256:{HEX}\"")).unwrap();
        assert_eq!(ok.as_str(), format!("sha256:{HEX}"));
        assert!(serde_json::from_str::<Digest>("\"sha256:nothex\"").is_err());
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            format!("\"sha256:{HEX}\"")
        );
    }

    #[test]
    fn host_linux_uses_oci_architecture_names() {
        let p = OciPlatform::host_linux();
        assert_eq!(p.os, "linux");
        assert_ne!(p.architecture, "x86_64");
        assert_ne!(p.architecture, "aarch64");
        assert_eq!(oci_architecture("x86_64"), "amd64");
        assert_eq!(oci_architecture("aarch64"), "arm64");
    }

    fn platform(os: &str, arch: &str, variant: Option<&str>) -> OciPlatform {
        OciPlatform {
            os: os.into(),
            architecture: arch.into(),
            variant: variant.map(str::to_string),
        }
    }

    #[test]
    fn platform_matching() {
        let amd = platform("linux", "amd64", None);
        let arm = platform("linux", "arm64", None);
        assert!(amd.matches(&amd));
        assert!(!amd.matches(&arm));
        assert!(!amd.matches(&platform("windows", "amd64", None)));
        assert!(arm.matches(&platform("linux", "arm64", Some("v8"))));
        assert!(!arm.matches(&platform("linux", "arm64", Some("v7"))));
        assert!(platform("linux", "arm", None).matches(&platform("linux", "arm", Some("v7"))));
        assert_eq!(
            platform("linux", "arm64", Some("v8")).to_string(),
            "linux/arm64/v8"
        );
    }

    #[test]
    fn identity_round_trips_and_records_its_source() {
        let identity = ImageIdentity {
            reference: "localhost:5000/awman-x:latest".into(),
            manifest_digest: Digest::parse(&format!("sha256:{HEX}")).unwrap(),
            config_digest: Digest::parse(&format!("sha256:{HEX}")).unwrap(),
            platform: OciPlatform::host_linux(),
            source: ImageSourceKind::Registry,
        };
        let json = serde_json::to_string(&identity).unwrap();
        let back: ImageIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, identity);
        let mut other = identity.clone();
        other.source = ImageSourceKind::DockerStore;
        assert_ne!(other, identity);
    }
}
