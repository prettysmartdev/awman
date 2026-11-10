//! Shared command and prompt data for frontend presenters.
use crate::command::dispatch::{catalogue::*, frontend_action::FrontendAction};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub fn catalogue() -> Value {
    fn node(spec: &CommandSpec, path: Vec<&str>) -> Value {
        let flags: Vec<_> = spec
            .flags
            .iter()
            .filter(|f| {
                matches!(
                    f.frontends,
                    FrontendVisibility::All
                        | FrontendVisibility::CliAndTui
                        | FrontendVisibility::TuiOnly
                )
            })
            .map(|f| {
                let default = match f.default {
                    FlagDefault::None | FlagDefault::EmptyVec => None,
                    FlagDefault::Bool(value) => Some(value.to_string()),
                    FlagDefault::Str(value) => Some(value.into()),
                    FlagDefault::U16(value) => Some(value.to_string()),
                };
                let kind = match f.kind {
                    FlagKind::Bool => "bool",
                    FlagKind::VecString => "vec_string",
                    FlagKind::Enum(_) => "enum",
                    FlagKind::Path | FlagKind::OptionalPath => "path",
                    FlagKind::U16 | FlagKind::UsizeAtLeastOne => "number",
                    _ => "string",
                };
                let mut value = json!({
                    "long": f.long, "short": f.short, "help": f.help,
                    "kind": kind, "default": default,
                });
                if let FlagKind::Enum(values) = f.kind {
                    value["values"] = json!(values);
                }
                value
            })
            .collect();
        let children: Vec<_> = spec
            .subcommands
            .iter()
            .filter(|s| !s.internal)
            .map(|s| {
                let mut p = path.clone();
                p.push(s.name);
                node(s, p)
            })
            .collect();
        let arguments: Vec<_> = spec
            .arguments
            .iter()
            .map(|a| {
                json!({
                    "name": a.name, "help": a.help, "required": !a.optional,
                })
            })
            .collect();
        json!({
            "path": path, "help": spec.help, "long_help": spec.long_help,
            "aliases": spec.aliases, "flags": flags, "arguments": arguments,
            "subcommands": children,
        })
    }
    let commands: Vec<_> = CommandCatalogue::get()
        .root()
        .subcommands
        .iter()
        .filter(|s| !s.internal)
        .map(|s| node(s, vec![s.name]))
        .collect();
    let actions = [
        ("show_config", FrontendAction::ShowConfig),
        ("new_squad_task", FrontendAction::NewSquadTask),
        ("edit_squad_task", FrontendAction::EditSquadTask),
        ("remove_squad_task", FrontendAction::RemoveSquadTask),
        ("trigger_squad_task", FrontendAction::TriggerSquadTask),
        ("cancel_squad_run", FrontendAction::CancelSquadRun),
        ("pause_squad_task", FrontendAction::PauseSquadTask),
        ("resume_squad_task", FrontendAction::ResumeSquadTask),
        ("report_repo_readiness", FrontendAction::ReportRepoReadiness),
        ("watch_status", FrontendAction::WatchStatus),
    ]
    .into_iter()
    .map(|(k, a)| (k.to_string(), json!(a.path())))
    .collect::<serde_json::Map<_, _>>();
    let mut value = json!({"commands":commands,"actions":actions});
    let hash = Sha256::digest(serde_json::to_vec(&value).unwrap());
    value["etag"] = json!(hash.iter().map(|b| format!("{b:02x}")).collect::<String>());
    value
}
pub fn prompt(kind: &str, action: Option<&str>, name: &str) -> Option<Value> {
    use crate::command::prompts;
    if kind == "squad_task_confirm" {
        let action = match action? {
            "pause" => FrontendAction::PauseSquadTask,
            "resume" => FrontendAction::ResumeSquadTask,
            "trigger" => FrontendAction::TriggerSquadTask,
            "cancel" => FrontendAction::CancelSquadRun,
            "delete" => FrontendAction::RemoveSquadTask,
            _ => return None,
        };
        let p = prompts::squad_task_confirmation_view(action, name)?;
        return Some(
            json!({"title":p.title,"body":p.body,"choices":p.choices.iter().map(|c|json!({"key":c.key,"label":c.label,"value":format!("{:?}",c.value).to_ascii_lowercase()})).collect::<Vec<_>>(),"default_on_dismiss":"dismiss"}),
        );
    }
    let p = prompts::frontend_prompt(kind)?;
    Some(
        json!({"title":p.title,"body":p.body,"choices":p.choices.iter().map(|c|json!({"key":c.key,"label":c.label,"value":c.value})).collect::<Vec<_>>(),"default_on_dismiss":p.default_on_dismiss}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_matches_tui_hints_recursively() {
        fn check(nodes: &[Value]) {
            for node in nodes {
                let path = node["path"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap())
                    .collect::<Vec<_>>();
                let hint = CommandCatalogue::get().tui_hint_for(&path).unwrap();
                assert_eq!(hint.help, node["help"]);
                assert_eq!(
                    hint.flags,
                    node["flags"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|f| format!("--{}", f["long"].as_str().unwrap()))
                        .collect::<Vec<_>>()
                );
                check(node["subcommands"].as_array().unwrap());
            }
        }
        let value = catalogue();
        check(value["commands"].as_array().unwrap());
        assert_eq!(value, catalogue());
        assert!(!value["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == json!(["view"])));
    }
}
