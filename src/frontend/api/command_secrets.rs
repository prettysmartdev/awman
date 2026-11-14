//! Authenticated request-only adapter. No Serialize implementation exists.
use std::fmt;
use std::sync::Arc;

use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::data::secret::{CommandSecrets, SecretString};

#[derive(Debug, Default)]
pub(super) struct SecretsBody(pub Option<Arc<CommandSecrets>>);

struct WireSecret(SecretString);
impl<'de> Deserialize<'de> for WireSecret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // On owned JSON input String is moved straight into the secret wrapper.
        crate::data::secret::frontend_wire::deserialize(d).map(Self)
    }
}

impl<'de> Deserialize<'de> for SecretsBody {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct SecretsVisitor;
        impl<'de> Visitor<'de> for SecretsVisitor {
            type Value = SecretsBody;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of secret names to strings")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::new();
                while let Some(name) = map.next_key::<String>()? {
                    let value = map
                        .next_value::<WireSecret>()
                        .map_err(|_| M::Error::custom("secret values must be strings"))?;
                    entries.push((name, value.0));
                    if entries.len() > crate::data::secret::MAX_COMMAND_SECRETS {
                        return Err(M::Error::custom("at most 32 command secrets are accepted"));
                    }
                }
                let secrets = CommandSecrets::new(entries).map_err(M::Error::custom)?;
                Ok(SecretsBody(
                    (!secrets.is_empty()).then(|| Arc::new(secrets)),
                ))
            }
        }
        d.deserialize_map(SecretsVisitor).map_err(|error| {
            if error.to_string().starts_with("invalid type:") {
                D::Error::custom("secrets must be a map of names to strings")
            } else {
                error
            }
        })
    }
}

/// Decode secrets before constructing any generic JSON copy or fingerprint.
/// Other fields retain their exact shape for compatible idempotency hashes.
pub(super) struct RequestBody<const RUN: bool> {
    pub value: serde_json::Value,
    pub secrets: SecretsBody,
    /// A run's `session.git_auth` (WI 0145), decoded the same way.
    pub git_auth: super::git_auth::GitAuthBody,
}

impl<'de, const RUN: bool> Deserialize<'de> for RequestBody<RUN> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct RequestVisitor<const RUN: bool>;
        impl<'de, const RUN: bool> Visitor<'de> for RequestVisitor<RUN> {
            type Value = RequestBody<RUN>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a command request object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = serde_json::Map::new();
                let mut secrets = SecretsBody::default();
                let mut git_auth = super::git_auth::GitAuthBody::default();
                let mut seen_secret_field = false;
                let mut seen_session = false;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "git_auth" {
                        return Err(M::Error::custom(
                            "invalid_git_auth: git_auth belongs in the session",
                        ));
                    }
                    if RUN && key == "secrets" {
                        return Err(M::Error::custom(
                            "invalid_secrets: secrets belong in command.secrets",
                        ));
                    }
                    let value = if !RUN && key == "secrets" {
                        if seen_secret_field {
                            return Err(M::Error::custom(
                                "invalid_secrets: duplicate secrets field",
                            ));
                        }
                        seen_secret_field = true;
                        secrets = map
                            .next_value::<SecretsBody>()
                            .map_err(|e| M::Error::custom(format!("invalid_secrets: {e}")))?;
                        let names: Vec<_> = secrets
                            .0
                            .as_ref()
                            .map(|s| s.names().collect())
                            .unwrap_or_default();
                        serde_json::json!(names)
                    } else if RUN && key == "session" {
                        if seen_session {
                            return Err(M::Error::custom("duplicate session field"));
                        }
                        seen_session = true;
                        let session = map.next_value::<super::git_auth::SessionBody>()?;
                        git_auth = session.git_auth;
                        session.value
                    } else if RUN && key == "command" {
                        if seen_secret_field {
                            return Err(M::Error::custom("duplicate command field"));
                        }
                        seen_secret_field = true;
                        let command = map.next_value::<RequestBody<false>>()?;
                        secrets = command.secrets;
                        command.value
                    } else {
                        map.next_value()?
                    };
                    fields.insert(key, value);
                }
                Ok(RequestBody {
                    value: serde_json::Value::Object(fields),
                    secrets,
                    git_auth,
                })
            }
        }
        d.deserialize_map(RequestVisitor::<RUN>)
    }
}

pub(super) fn decode<const RUN: bool>(
    input: Result<axum::Json<RequestBody<RUN>>, axum::extract::rejection::JsonRejection>,
) -> Result<RequestBody<RUN>, super::error::ApiError> {
    input.map(|body| body.0).map_err(|error| {
        let message = error.body_text();
        if let Some((_, detail)) = message.split_once("invalid_secrets: ") {
            super::error::ApiError::invalid_secrets(detail)
        } else {
            super::git_auth::rejection(error)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn command_and_run_http_decoders_validate_secrets_before_generic_json() {
        use axum::extract::FromRequest;
        use axum::response::IntoResponse;
        use axum::{body::Body, http::Request, Json};
        let wire = r#"{"command":{"subcommand":"exec prompt","args":["go"],"secrets":{"TOKEN":"test-secret-http"}},"session":{}}"#;
        let request = Request::builder()
            .header("content-type", "application/json")
            .body(Body::from(wire))
            .unwrap();
        let body = Json::<RequestBody<true>>::from_request(request, &())
            .await
            .unwrap()
            .0;
        assert_eq!(
            body.secrets.0.unwrap().get("TOKEN").unwrap().expose(),
            "test-secret-http"
        );
        assert_eq!(body.value["command"]["secrets"], json!(["TOKEN"]));
        assert!(!body.value.to_string().contains("test-secret-http"));
        for wire in [
            r#"{"secrets":{"TOKEN":"test-secret-one","TOKEN":"test-secret-two"}}"#,
            r#"{"secrets":{"bad":"test-secret-invalid"}}"#,
            r#"{"secrets":123456789}"#,
        ] {
            let request = Request::builder()
                .header("content-type", "application/json")
                .body(Body::from(wire))
                .unwrap();
            let response =
                match decode(Json::<RequestBody<false>>::from_request(request, &()).await) {
                    Ok(_) => panic!("invalid secrets accepted"),
                    Err(error) => error.into_response(),
                };
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["code"], "invalid_secrets");
            assert!(!value.to_string().contains("test-secret"));
            assert!(!value.to_string().contains("123456789"));
        }
    }

    #[test]
    fn wire_values_are_typed_and_removed_before_fingerprinting() {
        let RequestBody { value, secrets, .. } = serde_json::from_value::<RequestBody<false>>(json!({"subcommand":"exec prompt","args":["go"],"secrets":{"TOKEN":"test-secret-wire","ALSO":"test-secret-other"}})).unwrap();
        assert_eq!(
            secrets.0.as_ref().unwrap().get("TOKEN").unwrap().expose(),
            "test-secret-wire"
        );
        assert_eq!(value["secrets"], json!(["ALSO", "TOKEN"]));
        assert!(!value.to_string().contains("test-secret"));
        assert!(!format!("{secrets:?}").contains("test-secret"));
        let changed = serde_json::from_value::<RequestBody<false>>(json!({"subcommand":"exec prompt","args":["go"],"secrets":{"ALSO":"test-secret-changed","TOKEN":"test-secret-replacement"}})).unwrap();
        assert_eq!(value, changed.value);
    }

    #[test]
    fn invalid_wire_secrets_have_the_stable_400_code_and_no_value_echo() {
        for raw in [
            json!({"bad-name":"test-secret-invalid"}),
            json!({"TOKEN":""}),
            json!({"TOKEN":"test-secret-\0bad"}),
            json!({"TOKEN":["test-secret-wrong-type"]}),
            json!("test-secret-not-a-map"),
        ] {
            let error = serde_json::from_value::<RequestBody<false>>(json!({"secrets":raw}))
                .err()
                .expect("invalid secrets rejected")
                .to_string();
            assert!(error.contains("invalid_secrets"));
            assert!(!error.contains("test-secret"));
        }
        assert!(serde_json::from_str::<SecretsBody>(
            r#"{"TOKEN":"test-secret-first","TOKEN":"test-secret-second"}"#
        )
        .is_err());
    }
}

#[cfg(test)]
mod misplaced_tests {
    use super::*;
    #[test]
    fn credentials_in_unsupported_positions_are_rejected_before_fingerprinting() {
        for wire in [
            r#"{"git_auth":{"kind":"token","token":"test-secret-misplaced"}}"#,
            r#"{"secrets":{"TOKEN":"test-secret-misplaced"}}"#,
            r#"{"command":{"git_auth":{"kind":"token","token":"test-secret-misplaced"}}}"#,
            r#"{"session":{"secrets":{"TOKEN":"test-secret-misplaced"}}}"#,
        ] {
            let error = match serde_json::from_str::<RequestBody<true>>(wire) {
                Ok(_) => panic!("misplaced secret accepted"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("invalid_secrets") || error.contains("invalid_git_auth"),
                "{error}"
            );
            assert!(!error.contains("test-secret"));
        }
        assert!(serde_json::from_str::<RequestBody<false>>(
            r#"{"git_auth":{"token":"test-secret"}}"#
        )
        .is_err());
        assert!(serde_json::from_str::<super::super::git_auth::SessionBody>(
            r#"{"secrets":{"TOKEN":"test-secret"}}"#
        )
        .is_err());
    }
}
