//! `ConfigOverrides` — config values that come from outside the files:
//! derived `AWMAN_*` environment variables and `--set key=value` arguments.
//!
//! The type lives here so the merge in `effective.rs` can name it; the
//! constructors that parse `--set` arguments and `AWMAN_*` variables
//! (`ConfigOverrides::parse_set_args`, `ConfigOverrides::from_env`) are added
//! by `env.rs` as an inherent `impl` block, next to the rest of the
//! environment handling.

use serde_json::Value;

/// True when a raw `--set key=value` argument configures how this process
/// reaches other servers (`remote.*`: target URLs and API keys). Such an
/// argument is local to the invoking process and must never be forwarded in
/// a remote command's payload, where it would hand one target's credential
/// to another server.
pub fn is_local_only_set_arg(arg: &str) -> bool {
    let key = arg.split('=').next().unwrap_or("").trim();
    key == "remote" || key.starts_with("remote.")
}

/// The `--set` arguments that are safe to forward to a remote server.
pub fn forwardable_set_args(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !is_local_only_set_arg(a))
        .cloned()
        .collect()
}

/// `(concrete dotted path, value)` pairs, already resolved against the key
/// table and parsed by `fields::parse_input`. Applied in order, so for the
/// same key the last pair wins. `Value::Null` resets a key to its built-in
/// default.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigOverrides(pub Vec<(String, Value)>);

impl ConfigOverrides {
    pub fn new(pairs: Vec<(String, Value)>) -> Self {
        Self(pairs)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Reproduce the typed override layer as argv for a detached daemon.
    pub fn set_args(&self) -> Vec<String> {
        fn scalar(value: &Value) -> String {
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string())
        }
        self.iter()
            .map(|(path, value)| {
                let value = match value {
                    Value::Array(values) => values.iter().map(scalar).collect::<Vec<_>>().join(","),
                    Value::Object(values) => values
                        .iter()
                        .map(|(k, v)| format!("{k}={}", scalar(v)))
                        .collect::<Vec<_>>()
                        .join(","),
                    value => scalar(value),
                };
                format!("{path}={value}")
            })
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(String, Value)> {
        self.0.iter()
    }
}
