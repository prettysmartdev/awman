//! Acceptance inventory checks, not a substitute for browser/TUI outcome parity.
use serde_json::Value;
use std::collections::BTreeSet;

fn matrix() -> Value {
    serde_json::from_str(include_str!("../../web/acceptance/action-matrix.json")).unwrap()
}

#[test]
fn matrix_rows_and_rendered_inventory_agree() {
    let matrix = matrix();
    assert_eq!(matrix["schema_version"], 1);
    let rows = matrix["rows"].as_array().unwrap();
    let mut ids = BTreeSet::new();
    for row in rows {
        let id = row["id"].as_str().unwrap();
        assert!(ids.insert(id), "duplicate matrix row {id}");
        for (field, value) in [
            ("daemon.kind", &row["daemon"]["kind"]),
            ("browser.availability", &row["browser"]["availability"]),
            ("status", &row["status"]),
            ("parity", &row["parity"]),
        ] {
            assert!(
                matrix["fields"][field].as_array().unwrap().contains(value),
                "{id}: invalid {field}"
            );
        }
        if row["status"] == "pending_dependency" {
            assert_eq!(
                row["capability"], "worker_pull",
                "{id}: only WI 0143 may be deferred"
            );
            assert!(row["browser"]["reason"].as_str().unwrap().contains("0143"));
        }
        if let Some(alt) = row["browser"]["alt_shortcut"].as_str() {
            assert!(
                alt.starts_with("Alt+") || alt.starts_with("Ctrl+"),
                "{id}: malformed shortcut {alt}"
            );
        }
    }
    let rendered: BTreeSet<_> = include_str!("../../web/acceptance/action-matrix.md")
        .lines()
        .filter_map(|line| line.strip_prefix("| `")?.split('`').next())
        .collect();
    assert_eq!(ids, rendered);
}

#[test]
fn dialog_shortcut_options_and_close_frame_match_the_protocol() {
    use awman::data::{dialog::DialogResponse, frontend_protocol::FrontendFrame};
    let matrix = matrix();
    let rows = matrix["rows"].as_array().unwrap();
    let row = |id| rows.iter().find(|r| r["id"] == id).unwrap();
    for (id, alt) in [
        ("dialog.secret_input", Value::Null),
        ("dialog.multiline_input", Value::from("Ctrl+S")),
        ("dialog.workflow_yolo_countdown", Value::from("Alt+W")),
        ("dialog.workflow_step_confirm", Value::from("Alt+W")),
        ("dialog.close_tab_confirm", Value::from("Alt+T")),
    ] {
        assert_eq!(row(id)["browser"]["alt_shortcut"], alt, "{id}");
    }
    let detail = row("dialog.close_tab_confirm")["daemon"]["detail"]
        .as_str()
        .unwrap();
    assert!(!detail.contains("| CloseTab |"));
    assert!(detail.contains("FrontendFrame::CloseTab{tab_id}"));
    assert!(detail.contains("local confirmation"));
    // This is a frontend operation, never an answer in the dialog enum.
    assert_eq!(
        serde_json::from_value::<DialogResponse>(serde_json::json!({"kind":"yes"})).unwrap(),
        DialogResponse::Yes
    );
    assert!(
        serde_json::from_value::<DialogResponse>(serde_json::json!({"kind":"close_tab"})).is_err()
    );
    let frame: FrontendFrame = serde_json::from_value(
        serde_json::json!({"type":"close_tab","tab_id":awman::data::ids::TabId::new()}),
    )
    .unwrap();
    assert!(matches!(frame, FrontendFrame::CloseTab { .. }));
}

#[test]
#[ignore = "WI 0143 worker_pull queue panel/pick/release implementation is absent; enable with real queue actions when it lands"]
fn worker_pull_matrix_outcomes_require_0143() {
    // An explicit failing reminder on --ignored; an empty ignored test could
    // accidentally become a green acceptance gate without exercising anything.
    panic!(
        "WI 0143: implement queue.panel, queue.pick and queue.release_on_close outcome comparisons"
    );
}
