//! `git_auth` on `POST /v1/worker/sessions` and `/v1/worker/runs` `session`
//! (WI 0145). Request-only: values are read straight into `SecretString`
//! before any generic JSON copy (and therefore any idempotency fingerprint)
//! exists; the generic copy keeps only `{kind, username}`. No `Serialize`.
use std::fmt;
use std::sync::Arc;

use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::data::git_transport::{GitAuthKind, GitAuthSpec};
use crate::data::secret::SecretString;

/// Error prefix the HTTP decoder maps to `400 invalid_git_auth`.
pub(super) const INVALID_GIT_AUTH: &str = "invalid_git_auth: ";

#[derive(Debug, Default)]
pub(super) struct GitAuthBody(pub Option<Arc<GitAuthSpec>>);

impl GitAuthBody {
    /// What participates in idempotency: the kind and username, never the secret.
    pub fn canonical(&self) -> serde_json::Value {
        match &self.0 {
            Some(spec) => {
                serde_json::json!({"kind": spec.kind().as_str(), "username": spec.username()})
            }
            None => serde_json::Value::Null,
        }
    }
}

struct WireSecret(SecretString);
impl<'de> Deserialize<'de> for WireSecret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::data::secret::frontend_wire::deserialize(d).map(Self)
    }
}

impl<'de> Deserialize<'de> for GitAuthBody {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct AuthVisitor;
        impl<'de> Visitor<'de> for AuthVisitor {
            type Value = GitAuthBody;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a git_auth object")
            }
            fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
                Ok(GitAuthBody(None))
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let (mut kind, mut username, mut token, mut password) = (None, None, None, None);
                while let Some(key) = map.next_key::<String>()? {
                    let duplicate = || M::Error::custom(format!("duplicate field {key}"));
                    match key.as_str() {
                        "kind" if kind.is_none() => kind = Some(map.next_value::<String>()?),
                        "username" if username.is_none() => {
                            username = Some(map.next_value::<String>()?)
                        }
                        "token" if token.is_none() => {
                            token =
                                Some(map.next_value::<WireSecret>().map_err(|_| {
                                    M::Error::custom("git_auth token must be a string")
                                })?)
                        }
                        "password" if password.is_none() => {
                            password = Some(map.next_value::<WireSecret>().map_err(|_| {
                                M::Error::custom("git_auth password must be a string")
                            })?)
                        }
                        "kind" | "username" | "token" | "password" => return Err(duplicate()),
                        _ => return Err(M::Error::custom("unknown git_auth field")),
                    }
                }
                let kind = kind.ok_or_else(|| M::Error::custom("git_auth requires kind"))?;
                let kind = GitAuthKind::parse(&kind).ok_or_else(|| {
                    M::Error::custom("git_auth kind must be \"token\" or \"basic\"")
                })?;
                let secret = match (kind, token, password) {
                    (GitAuthKind::Token, Some(token), None) => token.0,
                    (GitAuthKind::Basic, None, Some(password)) => password.0,
                    (GitAuthKind::Token, _, _) => {
                        return Err(M::Error::custom(
                            "git_auth kind \"token\" requires token (and no password)",
                        ))
                    }
                    (GitAuthKind::Basic, _, _) => {
                        return Err(M::Error::custom(
                            "git_auth kind \"basic\" requires password (and no token)",
                        ))
                    }
                };
                GitAuthSpec::new(kind, username, secret)
                    .map(|spec| GitAuthBody(Some(Arc::new(spec))))
                    .map_err(M::Error::custom)
            }
        }
        d.deserialize_any(AuthVisitor).map_err(|error| {
            let message = error.to_string();
            if message.starts_with("invalid type:") {
                D::Error::custom(format!("{INVALID_GIT_AUTH}git_auth must be an object"))
            } else if message.starts_with(INVALID_GIT_AUTH) {
                error
            } else {
                D::Error::custom(format!("{INVALID_GIT_AUTH}{message}"))
            }
        })
    }
}

/// A session request body with `git_auth` decoded and reduced to
/// `{kind, username}` in the generic copy.
pub(super) struct SessionBody {
    pub value: serde_json::Value,
    pub git_auth: GitAuthBody,
}

impl<'de> Deserialize<'de> for SessionBody {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct SessionVisitor;
        impl<'de> Visitor<'de> for SessionVisitor {
            type Value = SessionBody;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a session request object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = serde_json::Map::new();
                let mut git_auth = GitAuthBody::default();
                let mut seen = false;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "secrets" {
                        return Err(M::Error::custom(
                            "invalid_secrets: secrets belong in command.secrets",
                        ));
                    }
                    let value = if key == "git_auth" {
                        if seen {
                            return Err(M::Error::custom(format!(
                                "{INVALID_GIT_AUTH}duplicate git_auth field"
                            )));
                        }
                        seen = true;
                        git_auth = map.next_value::<GitAuthBody>()?;
                        git_auth.canonical()
                    } else {
                        map.next_value()?
                    };
                    fields.insert(key, value);
                }
                Ok(SessionBody {
                    value: serde_json::Value::Object(fields),
                    git_auth,
                })
            }
        }
        d.deserialize_map(SessionVisitor)
    }
}

pub(super) fn decode(
    input: Result<axum::Json<SessionBody>, axum::extract::rejection::JsonRejection>,
) -> Result<SessionBody, super::error::ApiError> {
    input.map(|body| body.0).map_err(rejection)
}

/// Map a JSON rejection carrying a `git_auth` validation error to
/// `400 invalid_git_auth` (the message never contains a value).
pub(super) fn rejection(error: axum::extract::rejection::JsonRejection) -> super::error::ApiError {
    let message = error.body_text();
    match message.split_once(INVALID_GIT_AUTH) {
        Some((_, detail)) => super::error::ApiError::invalid_git_auth(
            detail
                .rsplit_once(" at line ")
                .map_or(detail, |(head, _)| head)
                .to_string(),
        ),
        None => super::error::ApiError::invalid_request(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn git_auth_is_typed_and_reduced_to_kind_and_username_before_fingerprinting() {
        let body: SessionBody = serde_json::from_value(json!({
            "session_type": "remote",
            "repo_url": "https://git.example/o/r.git",
            "git_auth": {"kind": "token", "token": "test-secret-session-token"}
        }))
        .unwrap();
        let spec = body.git_auth.0.as_ref().unwrap();
        assert_eq!(spec.secret().expose(), "test-secret-session-token");
        assert_eq!(
            body.value["git_auth"],
            json!({"kind": "token", "username": "x-access-token"})
        );
        assert!(!body.value.to_string().contains("test-secret"));
        let rotated: SessionBody = serde_json::from_value(json!({
            "session_type": "remote",
            "repo_url": "https://git.example/o/r.git",
            "git_auth": {"kind": "token", "token": "test-secret-other-token"}
        }))
        .unwrap();
        assert_eq!(body.value, rotated.value);
        let basic: SessionBody = serde_json::from_value(json!({
            "git_auth": {"kind": "basic", "username": "alice", "password": "test-secret-pw"}
        }))
        .unwrap();
        assert_eq!(
            basic.value["git_auth"],
            json!({"kind": "basic", "username": "alice"})
        );
        assert!(!format!("{:?}", basic.git_auth).contains("test-secret"));
    }

    #[test]
    fn invalid_git_auth_is_refused_without_echoing_a_value() {
        for raw in [
            json!({"kind": "token"}),
            json!({"kind": "basic", "password": "test-secret-a"}),
            json!({"kind": "bearer", "token": "test-secret-b"}),
            json!({"kind": "token", "token": "test-secret-c", "password": "test-secret-d"}),
            json!({"kind": "token", "token": "test-secret-e", "extra": 1}),
            json!({"kind": "token", "token": ["test-secret-f"]}),
            json!({"kind": "token", "token": "test-secret-\ng"}),
            json!("test-secret-h"),
        ] {
            let error = serde_json::from_value::<SessionBody>(json!({ "git_auth": raw }))
                .err()
                .expect("invalid git_auth refused")
                .to_string();
            assert!(error.contains(INVALID_GIT_AUTH), "{error}");
            assert!(!error.contains("test-secret"), "{error}");
        }
    }
}
