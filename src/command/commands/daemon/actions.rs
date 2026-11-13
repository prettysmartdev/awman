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
    /// Free text, sent as a JSON string.
    Text,
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
/// `?state=` of `GET /v1/worker/leases` (WI 0143).
const LEASE_STATES: &[&str] = &[
    "active",
    "claimed",
    "preparing",
    "running",
    "awaiting_input",
    "pushing",
    "reporting",
    "done",
    "released",
    "lost",
];
/// WSP release reasons (WI 0143).
const RELEASE_REASONS: &[&str] = &["user_declined", "ineligible", "shutdown", "error"];
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
        name: "launch_contexts.list",
        method: "GET",
        path: "/v1/engine/launch_contexts",
        args: &[],
        doc: "List authorised launch contexts (no environment values)",
    },
    Action {
        name: "launch_contexts.create",
        method: "POST",
        path: "/v1/engine/launch_contexts",
        args: &[
            optional_body("label", ArgKind::Name),
            optional_body("default", ArgKind::Json),
        ],
        doc: "Capture this shell and directory as a launch context",
    },
    Action {
        name: "launch_contexts.refresh",
        method: "POST",
        path: "/v1/engine/launch_contexts/{id}/refresh",
        args: &[arg("id", Location::Path, ArgKind::Id("lc"))],
        doc: "Refresh environment from the context root",
    },
    Action {
        name: "launch_contexts.delete",
        method: "DELETE",
        path: "/v1/engine/launch_contexts/{id}",
        args: &[arg("id", Location::Path, ArgKind::Id("lc"))],
        doc: "Remove a launch context",
    },
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
        name: "browsers.list",
        method: "GET",
        path: "/v1/browsers",
        args: PAGE,
        doc: "Paired browser grants: origin, listener, expiry (never the token)",
    },
    Action {
        name: "browsers.delete",
        method: "DELETE",
        path: "/v1/browsers/{id}",
        args: &[arg("id", Location::Path, ArgKind::Id("brw"))],
        doc: "Revoke a browser grant at once and close its connections (id=brw_…)",
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
        name: "leases.list",
        method: "GET",
        path: "/v1/worker/leases",
        args: &[
            optional("server", ArgKind::Name),
            optional("state", ArgKind::Enum(LEASE_STATES)),
            optional("limit", ArgKind::Limit),
            optional("cursor", ArgKind::Cursor),
        ],
        doc: "Worker leases, newest claim first (state=active for held ones)",
    },
    Action {
        name: "leases.release",
        method: "POST",
        path: "/v1/worker/leases/{id}/release",
        args: &[
            arg("id", Location::Path, ArgKind::Id("lease")),
            optional_body("reason", ArgKind::Enum(RELEASE_REASONS)),
            optional_body("message", ArgKind::Text),
        ],
        doc: "Give a lease back to its work server (id=lease_…; reason defaults to user_declined)",
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
            "worker: GET /v1/worker/commands?status=running, then jobs.cancel id=<each> (pushed and pulled jobs); then GET /v1/worker/leases?state=active and leases.release id=<each> reason=error for leases whose job was not cancelled",
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
        ArgKind::Text => !value.is_empty(),
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
        ArgKind::Text => "non-empty text".to_string(),
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
