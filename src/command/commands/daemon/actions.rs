//! `awman daemon api` actions: a client-side table of friendly names for
//! individual daemon routes. The server has no action registry; each action
//! is exactly one request, which `--raw` reproduces (`--list` prints it).
//!
//! Arguments are checked here, before anything is sent: an unknown action, an
//! unknown key or a mistyped value is refused locally.
use crate::command::error::CommandError;
use serde::{Deserialize, Serialize};

/// Where an argument goes in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Path,
    Query,
    Body,
}

/// What an argument's value must look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// `squad`, `worker` or `engine`.
    Feature,
    /// A name following the config map-name rule (`[a-z0-9][a-z0-9_-]{0,62}`).
    Name,
    /// A typed resource ID with this prefix (`key`, `grant`, …).
    Id(&'static str),
    /// A positive page size.
    Limit,
    /// An opaque page cursor.
    Cursor,
    /// A key in the annotation filter.
    Annotation,
    /// A serialized request or response value.
    Json,
    /// One value from a fixed enum.
    Enum(&'static [&'static str]),
    /// A terminal owner: `cmd_`, `tab_`, `lease_`, `run_` or `task_` (WI 0141).
    Owner,
}

#[derive(Debug, Clone, Copy)]
pub struct ActionArg {
    pub name: &'static str,
    pub required: bool,
    pub where_: Location,
    pub kind: ArgKind,
}

#[derive(Debug)]
pub struct Action {
    pub name: &'static str,
    pub method: &'static str,
    /// The route; `{arg}` segments are filled from path arguments.
    pub path: &'static str,
    pub args: &'static [ActionArg],
    pub doc: &'static str,
}

const fn arg(name: &'static str, where_: Location, kind: ArgKind) -> ActionArg {
    ActionArg {
        name,
        required: true,
        where_,
        kind,
    }
}
const fn optional(name: &'static str, kind: ArgKind) -> ActionArg {
    ActionArg {
        name,
        required: false,
        where_: Location::Query,
        kind,
    }
}
const fn optional_body(name: &'static str, kind: ArgKind) -> ActionArg {
    ActionArg {
        name,
        required: false,
        where_: Location::Body,
        kind,
    }
}

const PAGE: &[ActionArg] = &[
    optional("limit", ArgKind::Limit),
    optional("cursor", ArgKind::Cursor),
];
const ANNOTATION: ActionArg = ActionArg {
    name: "annotation",
    required: false,
    where_: Location::Query,
    kind: ArgKind::Annotation,
};
const UI_REQUESTS: &[&str] = &[
    "open_tab",
    "next_tab",
    "prev_tab",
    "close_tab",
    "set_container_window",
    "open_overview",
    "focus_workflow_window",
    "focus_work",
];

pub const ACTIONS: &[Action] = &[
    Action {
        name: "features.list",
        method: "GET",
        path: "/v1/features",
        args: &[],
        doc: "Every feature and its state",
    },
    Action {
        name: "features.get",
        method: "GET",
        path: "/v1/features/{feature}",
        args: &[arg("feature", Location::Path, ArgKind::Feature)],
        doc: "One feature's state",
    },
    Action {
        name: "keys.list",
        method: "GET",
        path: "/v1/keys",
        args: PAGE,
        doc: "Network API keys (never the key material)",
    },
    Action {
        name: "keys.create",
        method: "POST",
        path: "/v1/keys",
        args: &[arg("name", Location::Body, ArgKind::Name)],
        doc: "Create a named network API key; the key is shown once",
    },
    Action {
        name: "keys.delete",
        method: "DELETE",
        path: "/v1/keys/{id}",
        args: &[arg("id", Location::Path, ArgKind::Id("key"))],
        doc: "Revoke a network API key at once",
    },
    Action {
        name: "grants.list",
        method: "GET",
        path: "/v1/grants",
        args: PAGE,
        doc: "Remembered prerequisite grants",
    },
    Action {
        name: "grants.delete",
        method: "DELETE",
        path: "/v1/grants/{id}",
        args: &[arg("id", Location::Path, ArgKind::Id("grant"))],
        doc: "Delete one prerequisite grant",
    },
    Action {
        name: "jobs.cancel",
        method: "POST",
        path: "/v1/worker/commands/{id}/cancel",
        args: &[arg("id", Location::Path, ArgKind::Id("cmd"))],
        doc: "Cancel one worker command (id=cmd_…)",
    },
    Action {
        name: "jobs.cancel",
        method: "POST",
        path: "/v1/squad/runs/{id}/cancel",
        args: &[arg("id", Location::Path, ArgKind::Id("run"))],
        doc: "Cancel one squad run (id=run_…)",
    },
    Action {
        name: "terminals.list",
        method: "GET",
        path: "/v1/terminals",
        args: &[
            optional("owner_id", ArgKind::Owner),
            optional("limit", ArgKind::Limit),
            optional("cursor", ArgKind::Cursor),
        ],
        doc: "Agent terminals, newest first (owner_id=cmd_…|run_…|task_…|tab_…)",
    },
    Action {
        name: "terminals.get",
        method: "GET",
        path: "/v1/terminals/{id}",
        args: &[arg("id", Location::Path, ArgKind::Id("term"))],
        doc: "One terminal record (id=term_…)",
    },
    Action {
        name: "instances.list",
        method: "GET",
        path: "/v1/engine/instances",
        args: &[],
        doc: "Engine instances (repeat annotation.<key>=<value> to filter)",
    },
    Action {
        name: "instances.get",
        method: "GET",
        path: "/v1/engine/instances/{id}",
        args: &[arg("id", Location::Path, ArgKind::Id("ins"))],
        doc: "One engine instance and its tabs/frontends",
    },
    Action {
        name: "dialogs.list",
        method: "GET",
        path: "/v1/engine/instances/{id}/dialogs",
        args: &[arg("id", Location::Path, ArgKind::Id("ins"))],
        doc: "Pending dialogs for an engine instance",
    },
    Action {
        name: "dialogs.answer",
        method: "POST",
        path: "/v1/engine/instances/{id}/dialogs/{dialog_id}/answer",
        args: &[
            arg("id", Location::Path, ArgKind::Id("ins")),
            arg("dialog_id", Location::Path, ArgKind::Id("dlg")),
            arg("response", Location::Body, ArgKind::Json),
        ],
        doc: "Answer a pending dialog (response is a DialogResponse JSON value)",
    },
    Action {
        name: "ui.request",
        method: "POST",
        path: "/v1/engine/instances/{id}/ui_requests",
        args: &[
            arg("id", Location::Path, ArgKind::Id("ins")),
            arg("request", Location::Body, ArgKind::Enum(UI_REQUESTS)),
            optional_body("data", ArgKind::Json),
        ],
        doc: "Send a UiRequest selector and optional JSON payload to an engine instance",
    },
];

/// Conveniences composed from the actions above, documented so `--raw` users
/// can reproduce them.
#[derive(Debug)]
pub struct Composition {
    pub name: &'static str,
    pub steps: &'static [&'static str],
    pub doc: &'static str,
}

pub const COMPOSITIONS: &[Composition] = &[
    Composition {
        name: "jobs.cancel feature=<worker|squad>",
        steps: &[
            "worker: GET /v1/worker/commands?status=running, then jobs.cancel id=<each>",
            "squad: GET /v1/squad/tasks, GET /v1/squad/tasks/{id}/runs?status=running, then jobs.cancel id=<each>",
        ],
        doc: "Cancel every running job of a feature, following every next_cursor",
    },
    Composition {
        name: "grants.delete all",
        steps: &["grants.list", "grants.delete id=<each grant>"],
        doc: "Delete every prerequisite grant",
    },
];

/// One HTTP request to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<serde_json::Value>,
}

impl Request {
    pub fn get(path: impl Into<String>) -> Self {
        Self::new("GET", path, None)
    }
    pub fn post(path: impl Into<String>, body: Option<serde_json::Value>) -> Self {
        Self::new("POST", path, body)
    }
    pub fn new(method: &str, path: impl Into<String>, body: Option<serde_json::Value>) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            query: Vec::new(),
            body,
        }
    }
    pub fn with_query(mut self, key: &str, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }
    /// The request target: the path plus the percent-encoded query.
    pub fn target(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        let mut url = reqwest::Url::parse("http://localhost/").expect("static URL");
        url.query_pairs_mut()
            .extend_pairs(self.query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        format!("{}?{}", self.path, url.query().unwrap_or_default())
    }
}

fn invalid(message: impl Into<String>) -> CommandError {
    CommandError::Other(message.into())
}

/// Whether `name` is an action (or the target of a composition).
pub fn is_action(name: &str) -> bool {
    ACTIONS.iter().any(|a| a.name == name)
}

/// Check one value against its kind.
fn check(action: &str, arg: &ActionArg, value: &str) -> Result<(), CommandError> {
    let ok = match arg.kind {
        ArgKind::Feature => crate::data::daemon_state::FeatureName::parse(value).is_some(),
        ArgKind::Name => crate::data::config::fields::is_valid_map_name(value),
        ArgKind::Id(prefix) => id_has_prefix(value, prefix),
        ArgKind::Limit => value.parse::<u32>().is_ok_and(|n| n > 0),
        ArgKind::Cursor => !value.is_empty() && !value.contains(['/', '#']),
        ArgKind::Annotation => crate::data::config::fields::is_valid_map_name(value),
        ArgKind::Json => serde_json::from_str::<serde_json::Value>(value).is_ok(),
        ArgKind::Enum(values) => values.contains(&value),
        ArgKind::Owner => ["cmd", "tab", "lease", "run", "task"]
            .iter()
            .any(|prefix| id_has_prefix(value, prefix)),
    };
    if ok {
        return Ok(());
    }
    let expected = match arg.kind {
        ArgKind::Feature => "squad, worker or engine".to_string(),
        ArgKind::Name => "a lowercase name: [a-z0-9][a-z0-9_-]{0,62}".to_string(),
        ArgKind::Id(prefix) => format!("an id {prefix}_<32 hex>"),
        ArgKind::Limit => "a positive integer".to_string(),
        ArgKind::Cursor => "a cursor from a previous next_cursor".to_string(),
        ArgKind::Annotation => "a lowercase annotation name".to_string(),
        ArgKind::Json => "valid JSON".to_string(),
        ArgKind::Enum(values) => values.join(" | "),
        ArgKind::Owner => "an id cmd_|tab_|lease_|run_|task_<32 hex>".to_string(),
    };
    Err(invalid(format!(
        "{action}: invalid {}={value:?} (expected {expected})",
        arg.name
    )))
}

fn id_has_prefix(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The one request `name kv…` stands for. Compositions (`jobs.cancel
/// feature=`, `grants.delete all`) are expanded by the caller, not here.
pub fn plan(name: &str, kv: &[(String, String)]) -> Result<Request, CommandError> {
    let candidates: Vec<&Action> = ACTIONS.iter().filter(|a| a.name == name).collect();
    if candidates.is_empty() {
        return Err(invalid(format!(
            "unknown action {name:?}; `awman daemon api --list` lists them"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (key, _) in kv {
        if !seen.insert(key.as_str()) {
            return Err(invalid(format!("{name}: {key}= given more than once")));
        }
    }
    // Several actions may share a name (jobs.cancel); the id's prefix picks one.
    let action = if candidates.len() == 1 {
        candidates[0]
    } else {
        let id = kv.iter().find(|(k, _)| k == "id").map(|(_, v)| v.as_str());
        candidates
            .iter()
            .copied()
            .find(|a| {
                a.args.iter().any(|arg| {
                    matches!(arg.kind, ArgKind::Id(prefix) if id.is_some_and(|id| id_has_prefix(id, prefix)))
                })
            })
            .ok_or_else(|| {
                let prefixes: Vec<String> = candidates
                    .iter()
                    .flat_map(|a| a.args.iter())
                    .filter_map(|arg| match arg.kind {
                        ArgKind::Id(prefix) => Some(format!("id={prefix}_…")),
                        _ => None,
                    })
                    .collect();
                invalid(format!("{name} needs {}", prefixes.join(" or ")))
            })?
    };
    for arg in action.args {
        if arg.required && !kv.iter().any(|(k, _)| k == arg.name) {
            return Err(invalid(format!("{name} requires {}=", arg.name)));
        }
    }
    let mut request = Request::new(action.method, action.path, None);
    for (key, value) in kv {
        if name == "instances.list" {
            if let Some(annotation) = key.strip_prefix("annotation.") {
                check(name, &ANNOTATION, annotation)?;
                request
                    .query
                    .push(("annotation".into(), format!("{annotation}={value}")));
                continue;
            }
        }
        let arg = action.args.iter().find(|a| a.name == key).ok_or_else(|| {
            invalid(format!(
                "unknown argument {key:?} for {name}; `awman daemon api --list` shows its arguments"
            ))
        })?;
        check(name, arg, value)?;
        match arg.where_ {
            Location::Path => {
                request.path = request.path.replace(&format!("{{{key}}}"), value);
            }
            Location::Query => request.query.push((key.clone(), value.clone())),
            Location::Body => {
                let parsed: serde_json::Value = if matches!(arg.kind, ArgKind::Json) {
                    serde_json::from_str(value).map_err(|_| {
                        invalid(format!("{name}: invalid JSON {}={value:?}", arg.name))
                    })?
                } else {
                    serde_json::Value::String(value.clone())
                };
                if name == "ui.request" && key == "data" {
                    let Some(fields) = parsed.as_object() else {
                        return Err(invalid("ui.request data must be a JSON object"));
                    };
                    if fields.contains_key("request") {
                        return Err(invalid("ui.request data cannot override request="));
                    }
                    let body = request.body.get_or_insert_with(|| serde_json::json!({}));
                    for (field, value) in fields {
                        body[field] = value.clone();
                    }
                } else {
                    request.body.get_or_insert_with(|| serde_json::json!({}))[key.as_str()] =
                        parsed;
                }
            }
        }
    }
    Ok(request)
}

/// A `--raw METHOD /v1/path` request.
pub fn raw(
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<Request, CommandError> {
    let method = method.to_uppercase();
    if !["GET", "POST", "PATCH", "PUT", "DELETE"].contains(&method.as_str()) {
        return Err(invalid(format!(
            "--raw method must be GET, POST, PATCH, PUT or DELETE; got {method:?}"
        )));
    }
    if !path.starts_with("/v1/") || path.contains("//") || path.contains('#') {
        return Err(invalid("--raw needs a full /v1/… path"));
    }
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .map_err(|e| invalid(e.to_string()))?;
    Ok(Request {
        method,
        path: url.path().into(),
        query: url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
        body,
    })
}

/// The `--list` text: every action with its arguments and route, then the
/// compositions.
pub fn list() -> String {
    let rows: Vec<(String, String)> = ACTIONS
        .iter()
        .map(|action| {
            let args = action
                .args
                .iter()
                .map(|a| {
                    let value = match a.kind {
                        ArgKind::Id(prefix) => format!("{prefix}_…"),
                        ArgKind::Enum(values) => format!("<{}>", values.join("|")),
                        _ => "…".into(),
                    };
                    if a.required {
                        format!("{}={value}", a.name)
                    } else {
                        format!("[{}={value}]", a.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let left = if args.is_empty() {
                action.name.to_string()
            } else {
                format!("{} {args}", action.name)
            };
            (left, format!("{} {}", action.method, action.path))
        })
        .collect();
    let width = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for ((left, route), action) in rows.iter().zip(ACTIONS) {
        text.push_str(&format!(
            "{left:<width$}  → {route}    {}\n",
            action.doc,
            width = width
        ));
    }
    text.push_str("\ncompositions:\n");
    for c in COMPOSITIONS {
        text.push_str(&format!("{}  {}\n", c.name, c.doc));
        for step in c.steps {
            text.push_str(&format!("    {step}\n"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ids::{CommandId, GrantId, KeyId, RunId};

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A valid value for every argument kind.
    fn sample(kind: ArgKind) -> String {
        match kind {
            ArgKind::Feature => "worker".into(),
            ArgKind::Name => "ci-runner".into(),
            ArgKind::Id("key") => KeyId::new().to_string(),
            ArgKind::Id("grant") => GrantId::new().to_string(),
            ArgKind::Id("cmd") => CommandId::new().to_string(),
            ArgKind::Id("run") => RunId::new().to_string(),
            ArgKind::Id("term") => crate::data::ids::TerminalId::new().to_string(),
            ArgKind::Id("ins") => crate::data::ids::InstanceId::new().to_string(),
            ArgKind::Id("dlg") => crate::data::ids::DialogId::new().to_string(),
            ArgKind::Owner => CommandId::new().to_string(),
            ArgKind::Id(other) => panic!("no sample for {other}"),
            ArgKind::Limit => "5".into(),
            ArgKind::Cursor => "abc".into(),
            ArgKind::Annotation => "repo".into(),
            ArgKind::Json => r#"{"kind":"yes"}"#.into(),
            ArgKind::Enum(_) => "next_tab".into(),
        }
    }

    #[test]
    fn the_table_is_the_0140_action_set() {
        let mut names: Vec<&str> = ACTIONS.iter().map(|a| a.name).collect();
        names.dedup();
        assert_eq!(
            names,
            [
                "features.list",
                "features.get",
                "keys.list",
                "keys.create",
                "keys.delete",
                "grants.list",
                "grants.delete",
                "jobs.cancel",
                "terminals.list",
                "terminals.get",
                "instances.list",
                "instances.get",
                "dialogs.list",
                "dialogs.answer",
                "ui.request"
            ]
        );
    }

    /// Each action's request equals the `--raw` request `--list` documents.
    #[test]
    fn each_action_equals_its_raw_form() {
        for action in ACTIONS {
            let args: Vec<(String, String)> = action
                .args
                .iter()
                .map(|a| (a.name.to_string(), sample(a.kind)))
                .collect();
            let planned = plan(action.name, &args).unwrap();
            let mut path = action.path.to_string();
            let mut query = Vec::new();
            let mut body = None;
            for (arg, (_, value)) in action.args.iter().zip(&args) {
                match arg.where_ {
                    Location::Path => path = path.replace(&format!("{{{}}}", arg.name), value),
                    Location::Query => query.push(format!("{}={value}", arg.name)),
                    Location::Body => {
                        let parsed: serde_json::Value = if matches!(arg.kind, ArgKind::Json) {
                            serde_json::from_str(value).unwrap()
                        } else {
                            value.clone().into()
                        };
                        if action.name == "ui.request" && arg.name == "data" {
                            for (field, value) in parsed.as_object().unwrap() {
                                body.get_or_insert_with(|| serde_json::json!({}))[field] =
                                    value.clone();
                            }
                        } else {
                            body.get_or_insert_with(|| serde_json::json!({}))[arg.name] = parsed;
                        }
                    }
                }
            }
            let raw_path = if query.is_empty() {
                path
            } else {
                format!("{path}?{}", query.join("&"))
            };
            assert_eq!(
                planned,
                raw(action.method, &raw_path, body).unwrap(),
                "{}",
                action.name
            );
            assert_eq!(planned.target(), raw_path, "{}", action.name);
        }
    }

    #[test]
    fn keys_actions_address_the_key_routes() {
        let created = plan("keys.create", &kv(&[("name", "ci")])).unwrap();
        assert_eq!(
            created,
            raw("POST", "/v1/keys", Some(serde_json::json!({"name": "ci"}))).unwrap()
        );
        let id = KeyId::new().to_string();
        let deleted = plan("keys.delete", &kv(&[("id", &id)])).unwrap();
        assert_eq!(deleted.method, "DELETE");
        assert_eq!(deleted.path, format!("/v1/keys/{id}"));
        assert_eq!(
            plan("keys.list", &[]).unwrap(),
            raw("GET", "/v1/keys", None).unwrap()
        );
    }

    #[test]
    fn engine_actions_map_to_routes_and_json_bodies() {
        let id = crate::data::ids::InstanceId::new().to_string();
        let did = crate::data::ids::DialogId::new().to_string();
        let list = plan(
            "instances.list",
            &kv(&[("annotation.repo", "acme"), ("annotation.worktree", "one")]),
        )
        .unwrap();
        assert_eq!(list.path, "/v1/engine/instances");
        assert_eq!(
            list.query,
            kv(&[("annotation", "repo=acme"), ("annotation", "worktree=one")])
        );
        assert_eq!(
            plan("instances.get", &kv(&[("id", &id)])).unwrap(),
            Request::get(format!("/v1/engine/instances/{id}"))
        );
        assert_eq!(
            plan("dialogs.list", &kv(&[("id", &id)])).unwrap().path,
            format!("/v1/engine/instances/{id}/dialogs")
        );
        let response = r#"{"kind":"yes"}"#;
        let answer = plan(
            "dialogs.answer",
            &kv(&[("id", &id), ("dialog_id", &did), ("response", response)]),
        )
        .unwrap();
        assert_eq!(answer.method, "POST");
        assert_eq!(
            answer.path,
            format!("/v1/engine/instances/{id}/dialogs/{did}/answer")
        );
        assert_eq!(
            answer.body,
            Some(serde_json::json!({"response": {"kind":"yes"}}))
        );
        let ui = plan("ui.request", &kv(&[("id", &id), ("request", "next_tab")])).unwrap();
        assert_eq!(ui.path, format!("/v1/engine/instances/{id}/ui_requests"));
        assert_eq!(ui.body, Some(serde_json::json!({"request":"next_tab"})));
        let open = plan(
            "ui.request",
            &kv(&[
                ("id", &id),
                ("request", "open_tab"),
                ("data", r#"{"dir":"/tmp"}"#),
            ]),
        )
        .unwrap();
        assert_eq!(
            open.body,
            Some(serde_json::json!({"request":"open_tab", "dir":"/tmp"}))
        );
    }

    #[test]
    fn jobs_cancel_picks_the_route_by_id_prefix() {
        let cmd = CommandId::new().to_string();
        let run = RunId::new().to_string();
        assert_eq!(
            plan("jobs.cancel", &kv(&[("id", &cmd)])).unwrap().path,
            format!("/v1/worker/commands/{cmd}/cancel")
        );
        assert_eq!(
            plan("jobs.cancel", &kv(&[("id", &run)])).unwrap().path,
            format!("/v1/squad/runs/{run}/cancel")
        );
        for bad in [kv(&[]), kv(&[("id", "foo")]), kv(&[("id", "task_x")])] {
            let error = plan("jobs.cancel", &bad).unwrap_err().to_string();
            assert!(
                error.contains("jobs.cancel needs id=cmd_… or id=run_…"),
                "{error}"
            );
        }
    }

    #[test]
    fn unknown_actions_keys_and_mistyped_values_are_refused_locally() {
        let error = plan("status", &[]).unwrap_err().to_string();
        assert!(error.contains("unknown action \"status\""), "{error}");
        let error = plan("features.list", &kv(&[("x", "1")]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown argument \"x\""), "{error}");
        for (action, args) in [
            ("features.get", kv(&[("feature", "workers")])),
            ("keys.create", kv(&[("name", "Bad Name")])),
            (
                "keys.delete",
                kv(&[("id", "grant_00000000000000000000000000000000")]),
            ),
            ("grants.delete", kv(&[("id", "../x")])),
            ("keys.list", kv(&[("limit", "0")])),
            ("keys.list", kv(&[("limit", "ten")])),
        ] {
            let error = plan(action, &args).unwrap_err().to_string();
            assert!(error.contains("invalid"), "{action}: {error}");
        }
        let error = plan("keys.create", &[]).unwrap_err().to_string();
        assert!(error.contains("keys.create requires name="), "{error}");
        let dup = kv(&[("name", "a"), ("name", "b")]);
        assert!(plan("keys.create", &dup).is_err());
    }

    #[test]
    fn raw_requests_are_checked() {
        assert!(raw("FETCH", "/v1/status", None).is_err());
        assert!(raw("GET", "/status", None).is_err());
        assert!(raw("GET", "/v1//x", None).is_err());
        let request = raw("get", "/v1/worker/commands?status=running&limit=2", None).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v1/worker/commands");
        assert_eq!(request.query, kv(&[("status", "running"), ("limit", "2")]));
        assert_eq!(
            request.target(),
            "/v1/worker/commands?status=running&limit=2"
        );
    }

    #[test]
    fn list_prints_every_action_with_its_arguments_and_route() {
        let text = list();
        for action in ACTIONS {
            let line = text
                .lines()
                .find(|l| l.starts_with(action.name) && l.contains(action.path))
                .unwrap_or_else(|| panic!("{} missing:\n{text}", action.name));
            assert!(
                line.contains(&format!("→ {} {}", action.method, action.path)),
                "{line}"
            );
            for arg in action.args {
                assert!(line.contains(&format!("{}=", arg.name)), "{line}");
            }
        }
        assert!(text.contains("keys.create name=…"), "{text}");
        assert!(
            text.contains("jobs.cancel feature=<worker|squad>"),
            "{text}"
        );
    }
}
