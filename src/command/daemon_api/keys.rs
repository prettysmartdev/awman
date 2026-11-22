//! Owner-only named key operations. Neither hashes nor reusable plaintext are
//! included in list responses; create is the only plaintext disclosure.
use super::DaemonApi;
use crate::{
    data::{ids::KeyId, wire::List},
    engine::error::EngineError,
};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyResponse {
    pub id: KeyId,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}
#[derive(Serialize)]
pub struct KeyCreatedResponse {
    pub id: KeyId,
    pub name: String,
    pub key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("invalid key name")]
    InvalidName,
    #[error("key name is already in use")]
    NameTaken,
    #[error(transparent)]
    Engine(#[from] EngineError),
}
fn store(api: &DaemonApi) -> Result<&crate::engine::auth::api_keys::ApiKeyStore, KeyError> {
    Ok(api.runtime.keys().as_ref())
}
pub fn keys_list(api: &DaemonApi) -> Result<List<KeyResponse>, KeyError> {
    Ok(List {
        items: store(api)?
            .list()
            .into_iter()
            .map(|record| KeyResponse {
                id: record.id,
                name: record.name,
                created_at: record.created_at,
                last_used_at: record.last_used_at,
            })
            .collect(),
        next_cursor: None,
    })
}
pub fn keys_create(api: &DaemonApi, name: &str) -> Result<KeyCreatedResponse, KeyError> {
    if !crate::data::config::fields::is_valid_map_name(name) {
        return Err(KeyError::InvalidName);
    }
    let store = store(api)?;
    if store.list().iter().any(|record| record.name == name) {
        return Err(KeyError::NameTaken);
    }
    let (record, key) = store.create(name)?;
    Ok(KeyCreatedResponse {
        id: record.id,
        name: record.name,
        key: key.as_str().into(),
        created_at: record.created_at,
    })
}
pub fn keys_delete(api: &DaemonApi, id: &KeyId) -> Result<bool, KeyError> {
    Ok(store(api)?.delete(id)?)
}
