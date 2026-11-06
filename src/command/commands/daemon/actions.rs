//! Client-side aliases for individual resource requests. No daemon action endpoint exists.
use crate::command::daemon_api::Operation;
use crate::command::error::CommandError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Path,
    Query,
    Body,
}
#[derive(Debug, Clone, Copy)]
pub struct ActionArg {
    pub name: &'static str,
    pub required: bool,
    pub where_: Location,
}
#[derive(Debug)]
pub struct Action {
    pub name: &'static str,
    pub op: Operation,
    pub args: &'static [ActionArg],
    pub doc: &'static str,
}
const ID: &[ActionArg] = &[ActionArg {
    name: "id",
    required: true,
    where_: Location::Path,
}];
const FEATURE: &[ActionArg] = &[ActionArg {
    name: "feature",
    required: true,
    where_: Location::Path,
}];
const PAGE: &[ActionArg] = &[
    ActionArg {
        name: "limit",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "cursor",
        required: false,
        where_: Location::Query,
    },
];
const TASKS: &[ActionArg] = &[
    ActionArg {
        name: "name",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "limit",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "cursor",
        required: false,
        where_: Location::Query,
    },
];
const WORKER_LIST: &[ActionArg] = &[
    ActionArg {
        name: "status",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "setup_status",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "session_id",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "label",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "limit",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "cursor",
        required: false,
        where_: Location::Query,
    },
];
const RUNS: &[ActionArg] = &[
    ActionArg {
        name: "id",
        required: true,
        where_: Location::Path,
    },
    ActionArg {
        name: "status",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "limit",
        required: false,
        where_: Location::Query,
    },
    ActionArg {
        name: "cursor",
        required: false,
        where_: Location::Query,
    },
];
macro_rules! action {
    ($name:literal,$op:ident,$args:expr,$doc:literal) => {
        Action {
            name: $name,
            op: Operation::$op,
            args: $args,
            doc: $doc,
        }
    };
}
pub const ACTIONS: &[Action] = &[
    action!("status", CoreStatus, &[], "Daemon status and capabilities"),
    action!(
        "config.get",
        CoreConfigGet,
        &[],
        "Masked effective configuration"
    ),
    action!(
        "config.reload",
        CoreConfigReload,
        &[],
        "Validate and reload live settings"
    ),
    action!("features.list", CoreFeaturesList, PAGE, "Feature states"),
    action!(
        "features.get",
        CoreFeaturesGet,
        FEATURE,
        "One feature's state"
    ),
    action!(
        "grants.list",
        CoreGrantsList,
        PAGE,
        "Remembered prerequisite grants"
    ),
    action!(
        "grants.delete",
        CoreGrantsDelete,
        ID,
        "Delete a grant by grant_ ID"
    ),
    action!("jobs.cancel", WorkerCommandsCancel, ID, "Cancel a cmd_ job"),
    action!("jobs.cancel", SquadRunsCancel, ID, "Cancel a run_ job"),
    action!(
        "sessions.list",
        WorkerSessionsList,
        WORKER_LIST,
        "Worker sessions"
    ),
    action!("sessions.get", WorkerSessionsGet, ID, "One worker session"),
    action!(
        "sessions.delete",
        WorkerSessionsDelete,
        ID,
        "Close a worker session"
    ),
    action!(
        "commands.list",
        WorkerCommandsList,
        WORKER_LIST,
        "Worker commands"
    ),
    action!("commands.get", WorkerCommandsGet, ID, "One worker command"),
    action!(
        "commands.cancel",
        WorkerCommandsCancel,
        ID,
        "Cancel one command"
    ),
    action!(
        "tasks.list",
        SquadTasksList,
        TASKS,
        "Squad tasks, optionally filtered by name"
    ),
    action!("tasks.get", SquadTasksGet, ID, "One squad task"),
    action!(
        "tasks.delete",
        SquadTasksDelete,
        ID,
        "Delete one squad task"
    ),
    action!("runs.list", SquadRunsList, RUNS, "Runs of the task_ ID"),
    action!("runs.cancel", SquadRunsCancel, ID, "Cancel one run"),
];
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
            "commands.list status=running / tasks.list + runs.list status=running",
            "commands.cancel / runs.cancel for each id",
        ],
        doc: "List running jobs for the feature and cancel each; follow every next_cursor.",
    },
    Composition {
        name: "grants.delete all",
        steps: &["grants.list", "grants.delete id=<each grant>"],
        doc: "List all pages, then delete each grant by ID.",
    },
    Composition {
        name: "steps.approve",
        steps: &[],
        doc: "Reserved composition for later work items; no route or implementation in 0139.",
    },
    Composition {
        name: "overview.open",
        steps: &[],
        doc: "Reserved composition for later work items; no route or implementation in 0139.",
    },
];
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<serde_json::Value>,
}
fn invalid(message: impl Into<String>) -> CommandError {
    CommandError::Other(message.into())
}
pub fn plan(name: &str, kv: &[(String, String)]) -> Result<Request, CommandError> {
    let id = kv.iter().find(|(k, _)| k == "id").map(|(_, v)| v.as_str());
    if name == "jobs.cancel"
        && !kv.iter().any(|(k, v)| {
            (k == "feature" && matches!(v.as_str(), "worker" | "squad"))
                || (k == "id" && (v.starts_with("cmd_") || v.starts_with("run_")))
        })
    {
        return Err(invalid(
            "jobs.cancel needs id=cmd_…|run_… or feature=worker|squad",
        ));
    }
    let action = ACTIONS
        .iter()
        .find(|a| {
            a.name == name
                && (name != "jobs.cancel"
                    || match a.op {
                        Operation::WorkerCommandsCancel => {
                            id.is_some_and(|id| id.starts_with("cmd_"))
                        }
                        Operation::SquadRunsCancel => id.is_some_and(|id| id.starts_with("run_")),
                        _ => false,
                    })
        })
        .ok_or_else(|| invalid(format!("unknown action {name:?}; use --list")))?;
    let (method, path) = route(action.op);
    let mut request = Request {
        method: method.into(),
        path,
        query: vec![],
        body: None,
    };
    for arg in action.args {
        if arg.required && !kv.iter().any(|(k, _)| k == arg.name) {
            return Err(invalid(format!("{} requires {}=", action.name, arg.name)));
        }
    }
    for (key, value) in kv {
        let arg = action
            .args
            .iter()
            .find(|a| a.name == key)
            .ok_or_else(|| invalid(format!("unknown argument {key:?} for {}", action.name)))?;
        match arg.where_ {
            Location::Path => {
                if value.is_empty() || value.contains(['/', '?', '#', '%']) {
                    return Err(invalid("invalid path argument"));
                }
                request.path = request.path.replace(&format!("{{{key}}}"), value);
            }
            Location::Query => request.query.push((key.clone(), value.clone())),
            Location::Body => {
                let body = request.body.get_or_insert_with(|| serde_json::json!({}));
                body[key] = serde_json::from_str(value).unwrap_or_else(|_| value.clone().into());
            }
        }
    }
    Ok(request)
}
pub fn raw(
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<Request, CommandError> {
    let method = method.to_uppercase();
    if !["GET", "POST", "PATCH", "PUT", "DELETE"].contains(&method.as_str())
        || !path.starts_with("/v1/")
        || path.starts_with("//")
    {
        return Err(invalid("--raw requires METHOD and a full /v1/ path"));
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
pub fn list() -> String {
    let mut text = String::new();
    for action in ACTIONS {
        let (method, path) = route(action.op);
        text.push_str(&format!(
            "{} {} → {} {}\n",
            action.name,
            action
                .args
                .iter()
                .map(|a| format!("{}={}", a.name, if a.required { "…" } else { "(optional)" }))
                .collect::<Vec<_>>()
                .join(" "),
            method,
            path
        ));
    }
    for c in COMPOSITIONS {
        text.push_str(&format!("{} (composition): {}\n", c.name, c.doc));
    }
    text
}
/// Client resource addressing uses the transport-neutral operation name. The server's
/// registration table remains in the frontend, with no upward Layer 2 dependency.
pub fn route(op: Operation) -> (&'static str, String) {
    use Operation::*;
    if op == CoreStatus {
        return ("GET", "/v1/status".into());
    }
    if op == CoreConfigGet {
        return ("GET", "/v1/config".into());
    }
    if op == CoreConfigReload {
        return ("POST", "/v1/config/reload".into());
    }
    if op == SquadRunsList {
        return ("GET", "/v1/squad/tasks/{id}/runs".into());
    }
    let parts = op.name().split('.').collect::<Vec<_>>();
    let prefix = if parts[0] == "core" {
        String::new()
    } else {
        format!("/{0}", parts[0])
    };
    let method = match parts.last().copied().unwrap() {
        "list" | "get" => "GET",
        "delete" => "DELETE",
        "cancel" => "POST",
        _ => unreachable!("not a read/transition action"),
    };
    let mut path = format!("/v1{prefix}/{}", parts[1]);
    match parts[2] {
        "list" => {}
        "get" if op == CoreFeaturesGet => path.push_str("/{feature}"),
        "get" | "delete" => path.push_str("/{id}"),
        "cancel" => path.push_str("/{id}/cancel"),
        _ => unreachable!(),
    }
    (method, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_cancel_without_a_supported_selector_gets_a_specific_error() {
        for args in [
            Vec::<(String, String)>::new(),
            vec![("id".to_string(), "foo".to_string())],
        ] {
            let error = plan("jobs.cancel", &args).unwrap_err().to_string();
            assert!(
                error.contains("jobs.cancel needs id=cmd_…|run_… or feature=worker|squad"),
                "{error}"
            );
        }
    }
    #[test]
    fn each_action_equals_its_documented_raw_request() {
        for a in ACTIONS {
            let (method, path) = route(a.op);
            let kv: Vec<_> = a
                .args
                .iter()
                .filter(|a| a.required)
                .map(|a| {
                    (
                        a.name.to_string(),
                        if a.name == "feature" {
                            "worker"
                        } else {
                            "test"
                        }
                        .into(),
                    )
                })
                .collect();
            let kv = kv
                .into_iter()
                .map(|(k, v)| {
                    let v = if k == "id" {
                        match a.op.owner() {
                            crate::command::daemon_api::Feature::Squad => {
                                if a.op == Operation::SquadRunsCancel {
                                    crate::data::ids::RunId::new().to_string()
                                } else {
                                    crate::data::ids::TaskId::new().to_string()
                                }
                            }
                            _ => {
                                if a.op == Operation::CoreGrantsDelete {
                                    crate::data::ids::GrantId::new().to_string()
                                } else {
                                    crate::data::ids::CommandId::new().to_string()
                                }
                            }
                        }
                    } else {
                        v
                    };
                    (k, v)
                })
                .collect::<Vec<_>>();
            let planned = plan(a.name, &kv).unwrap();
            let expected_path = kv
                .iter()
                .fold(path.clone(), |p, (k, v)| p.replace(&format!("{{{k}}}"), v));
            assert_eq!(planned, raw(method, &expected_path, None).unwrap());
        }
    }
}
