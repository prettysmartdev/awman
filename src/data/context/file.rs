//! Metadata derived from a safely opened regular file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextFileMeta {
    pub path: String,
    #[serde(with = "crate::data::wire::bytes")]
    pub size_bytes: u64,
    pub sha256: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::data::wire::opt_timestamp"
    )]
    pub modified_at: Option<chrono::DateTime<chrono::Utc>>,
}
