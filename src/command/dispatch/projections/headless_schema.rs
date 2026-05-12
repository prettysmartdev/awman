//! REST-route + OpenAPI projection of the catalogue.

use serde_json::{json, Value};

use crate::command::dispatch::catalogue::{
    ArgumentKind, CommandCatalogue, CommandSpec, FlagKind, FrontendVisibility,
};

/// One row of the headless REST routing table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestRoute {
    /// HTTP method, always `POST` for command-style routes.
    pub method: &'static str,
    /// Path segment, e.g. `/v1/exec/workflow`.
    pub path: String,
    /// Underlying command path (catalogue lookup form).
    pub command_path: Vec<String>,
}

impl CommandCatalogue {
    /// Render every catalogue command as a `POST /v1/<path>` route.
    pub fn rest_route_table(&self) -> Vec<RestRoute> {
        let mut out = Vec::new();
        collect_routes(self.root(), &mut Vec::new(), &mut out);
        out
    }

    /// Render an OpenAPI-ish schema for the entire command surface.
    pub fn openapi_schema(&self) -> Value {
        let mut paths = serde_json::Map::new();
        for route in self.rest_route_table() {
            let spec = self
                .lookup(
                    &route
                        .command_path
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>(),
                )
                .expect("rest route must resolve");
            let mut params = Vec::new();
            for arg in spec.arguments {
                params.push(json!({
                    "name": arg.name,
                    "in": "body",
                    "required": !arg.optional,
                    "schema": json_kind_for_argument(arg.kind),
                    "description": arg.help,
                }));
            }
            for flag in spec.flags {
                if !flag_visible_to_headless(flag.frontends) {
                    continue;
                }
                params.push(json!({
                    "name": flag.long,
                    "in": "body",
                    "required": false,
                    "schema": json_kind_for_flag(flag.kind),
                    "description": flag.help,
                }));
            }
            paths.insert(
                route.path.clone(),
                json!({
                    "post": {
                        "summary": spec.help,
                        "operationId": route.command_path.join("_"),
                        "parameters": params,
                    }
                }),
            );
        }
        json!({
            "openapi": "3.0.0",
            "info": { "title": "amux headless API", "version": "1" },
            "paths": Value::Object(paths),
        })
    }
}

fn collect_routes(spec: &'static CommandSpec, path: &mut Vec<String>, out: &mut Vec<RestRoute>) {
    if spec.subcommands.is_empty() && !path.is_empty() {
        out.push(RestRoute {
            method: "POST",
            path: format!("/v1/{}", path.join("/")),
            command_path: path.clone(),
        });
    }
    for sub in spec.subcommands {
        path.push(sub.name.to_string());
        collect_routes(sub, path, out);
        path.pop();
    }
}

fn flag_visible_to_headless(v: FrontendVisibility) -> bool {
    matches!(v, FrontendVisibility::All)
}

fn json_kind_for_flag(kind: FlagKind) -> Value {
    match kind {
        FlagKind::Bool => json!({ "type": "boolean" }),
        FlagKind::String | FlagKind::OptionalString => json!({ "type": "string" }),
        FlagKind::Enum(values) => {
            json!({ "type": "string", "enum": values.iter().collect::<Vec<_>>() })
        }
        FlagKind::VecString => {
            json!({ "type": "array", "items": { "type": "string" } })
        }
        FlagKind::Path | FlagKind::OptionalPath => {
            json!({ "type": "string", "format": "path" })
        }
        FlagKind::U16 => json!({ "type": "integer", "minimum": 0, "maximum": 65535 }),
    }
}

fn json_kind_for_argument(kind: ArgumentKind) -> Value {
    match kind {
        ArgumentKind::String | ArgumentKind::OptionalString => json!({ "type": "string" }),
        ArgumentKind::Path | ArgumentKind::OptionalPath => {
            json!({ "type": "string", "format": "path" })
        }
        ArgumentKind::TrailingVarArgs => {
            json!({ "type": "array", "items": { "type": "string" } })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_table_contains_exec_workflow_route() {
        let cat = CommandCatalogue::get();
        let routes = cat.rest_route_table();
        assert!(routes
            .iter()
            .any(|r| r.path == "/v1/exec/workflow" && r.method == "POST"));
    }

    #[test]
    fn route_table_contains_remote_session_kill() {
        let cat = CommandCatalogue::get();
        let routes = cat.rest_route_table();
        assert!(routes.iter().any(|r| r.path == "/v1/remote/session/kill"));
    }

    #[test]
    fn openapi_schema_has_paths() {
        let cat = CommandCatalogue::get();
        let schema = cat.openapi_schema();
        let paths = schema.get("paths").unwrap().as_object().unwrap();
        assert!(paths.contains_key("/v1/exec/workflow"));
    }

    // Walk every leaf command in the catalogue and assert it appears in both
    // the rest_route_table and the openapi_schema paths.
    fn walk_and_verify_headless_leaf(
        _cat: &CommandCatalogue,
        spec: &'static crate::command::dispatch::catalogue::CommandSpec,
        path: Vec<String>,
        routes: &[RestRoute],
        schema_paths: &serde_json::Map<String, serde_json::Value>,
    ) {
        if !path.is_empty() && spec.subcommands.is_empty() {
            let route_path = format!("/v1/{}", path.join("/"));
            assert!(
                routes.iter().any(|r| r.path == route_path),
                "leaf {:?} missing from rest_route_table (expected path '{route_path}')",
                path
            );
            assert!(
                schema_paths.contains_key(&route_path),
                "leaf {:?} missing from openapi_schema (expected path '{route_path}')",
                path
            );
            // Method must be POST.
            let route = routes.iter().find(|r| r.path == route_path).unwrap();
            assert_eq!(
                route.method, "POST",
                "route method must be POST for leaf {:?}",
                path
            );
        }
        for sub in spec.subcommands {
            let mut new_path = path.clone();
            new_path.push(sub.name.to_string());
            walk_and_verify_headless_leaf(_cat, sub, new_path, routes, schema_paths);
        }
    }

    #[test]
    fn catalogue_headless_consistency_every_leaf_in_route_table_and_schema() {
        let cat = CommandCatalogue::get();
        let routes = cat.rest_route_table();
        let schema = cat.openapi_schema();
        let schema_paths = schema
            .get("paths")
            .expect("openapi_schema must have 'paths'")
            .as_object()
            .expect("paths must be an object");
        walk_and_verify_headless_leaf(cat, cat.root(), vec![], &routes, schema_paths);
    }

    #[test]
    fn route_table_contains_all_expected_leaf_routes() {
        let cat = CommandCatalogue::get();
        let routes = cat.rest_route_table();
        let expected_paths = &[
            "/v1/init",
            "/v1/ready",
            "/v1/chat",
            "/v1/specs/amend",
            "/v1/status",
            "/v1/config/show",
            "/v1/config/get",
            "/v1/config/set",
            "/v1/exec/prompt",
            "/v1/exec/workflow",
            "/v1/headless/start",
            "/v1/headless/kill",
            "/v1/headless/logs",
            "/v1/headless/status",
            "/v1/remote/run",
            "/v1/remote/session/start",
            "/v1/remote/session/kill",
            "/v1/new/spec",
            "/v1/new/workflow",
            "/v1/new/skill",
        ];
        for expected in expected_paths {
            assert!(
                routes.iter().any(|r| r.path == *expected),
                "route '{expected}' missing from rest_route_table"
            );
        }
    }

    #[test]
    fn openapi_schema_has_correct_structure() {
        let cat = CommandCatalogue::get();
        let schema = cat.openapi_schema();
        assert_eq!(schema["openapi"], "3.0.0");
        assert_eq!(schema["info"]["title"], "amux headless API");
        let paths = schema["paths"].as_object().unwrap();
        assert!(!paths.is_empty(), "schema must have at least one path");
    }
}
