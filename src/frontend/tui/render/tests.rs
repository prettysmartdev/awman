use super::command_box::{command_box_scroll_offset, truncate_middle};
use super::dialog::{cursor_window, render_config_show};
use super::execution_window::{apply_selection_highlight, capture_buffer_grid};
use super::sidebar::{git_file_line, render_git_sidebar, truncate_path};
use crate::frontend::tui::dialogs;
use crate::frontend::tui::git_sidebar::{GitDiffSummary, GitFileChangeType, GitFileEntry};
use crate::frontend::tui::tabs::TextSelection;
use ratatui::prelude::*;

/// Render a closure into a fresh `TestBackend` and return the resulting
/// buffer for cell-level assertions.
fn render_to_buffer(
    width: u16,
    height: u16,
    f: impl FnOnce(ratatui::layout::Rect, &mut ratatui::Frame),
) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| f(frame.area(), frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
    let area = *buf.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sample_summary() -> GitDiffSummary {
    GitDiffSummary {
        files: vec![
            GitFileEntry {
                path: "src/foo.rs".to_string(),
                change_type: GitFileChangeType::Modified,
                additions: 5,
                deletions: 2,
                binary: false,
            },
            GitFileEntry {
                path: "img.png".to_string(),
                change_type: GitFileChangeType::Added,
                additions: 0,
                deletions: 0,
                binary: true,
            },
        ],
        total_additions: 5,
        total_deletions: 2,
        branch: Some("main".to_string()),
    }
}

// ── git sidebar block ──────────────────────────────────────────────────

#[test]
fn sidebar_block_uses_rounded_green_border() {
    let buf = render_to_buffer(24, 8, |area, frame| {
        render_git_sidebar(frame, area, &Some(sample_summary()));
    });
    // Rounded corners are the '╭╮╰╯' glyph set.
    let top_left = buf.cell((0, 0)).unwrap();
    assert_eq!(top_left.symbol(), "\u{256d}", "top-left rounded corner");
    assert_eq!(buf.cell((23, 0)).unwrap().symbol(), "\u{256e}", "top-right");
    assert_eq!(
        buf.cell((0, 7)).unwrap().symbol(),
        "\u{2570}",
        "bottom-left"
    );
    assert_eq!(
        buf.cell((23, 7)).unwrap().symbol(),
        "\u{256f}",
        "bottom-right"
    );
    // Border style is green.
    assert_eq!(top_left.fg, Color::Green, "border must be green");
}

#[test]
fn sidebar_shows_totals_and_binary_suffix() {
    let buf = render_to_buffer(24, 8, |area, frame| {
        render_git_sidebar(frame, area, &Some(sample_summary()));
    });
    let text = buffer_text(&buf);
    assert!(text.contains("+5"), "totals additions shown: {text:?}");
    assert!(text.contains("-2"), "totals deletions shown: {text:?}");
    assert!(text.contains("foo.rs"), "file path shown: {text:?}");
    assert!(
        text.contains("(binary)"),
        "binary file gets a (binary) suffix: {text:?}"
    );
}

#[test]
fn sidebar_none_summary_shows_no_git_data() {
    let buf = render_to_buffer(24, 8, |area, frame| {
        render_git_sidebar(frame, area, &None);
    });
    assert!(buffer_text(&buf).contains("no git data"));
}

/// The top border row of the rendered buffer, as a string.
fn top_border_row(buf: &ratatui::buffer::Buffer) -> String {
    buffer_text(buf)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn sidebar_title_shows_branch_and_change_count() {
    let buf = render_to_buffer(30, 8, |area, frame| {
        render_git_sidebar(frame, area, &Some(sample_summary()));
    });
    let top = top_border_row(&buf);
    assert!(
        top.contains(" main: 2 changed "),
        "border title condenses git status: {top:?}"
    );
}

#[test]
fn sidebar_title_clean_when_no_changed_files() {
    let summary = GitDiffSummary {
        files: Vec::new(),
        total_additions: 0,
        total_deletions: 0,
        branch: Some("main".to_string()),
    };
    let buf = render_to_buffer(30, 8, |area, frame| {
        render_git_sidebar(frame, area, &Some(summary));
    });
    let top = top_border_row(&buf);
    assert!(top.contains(" main: clean "), "clean title: {top:?}");
}

#[test]
fn sidebar_title_present_even_without_git_data() {
    let buf = render_to_buffer(30, 8, |area, frame| {
        render_git_sidebar(frame, area, &None);
    });
    let top = top_border_row(&buf);
    assert!(
        top.contains(" git status "),
        "sidebar is titled even with no data: {top:?}"
    );
}

// ── git_file_line ──────────────────────────────────────────────────────

#[test]
fn git_file_line_added_is_green_with_stat_prefix() {
    let entry = GitFileEntry {
        path: "a.rs".to_string(),
        change_type: GitFileChangeType::Added,
        additions: 3,
        deletions: 0,
        binary: false,
    };
    let line = git_file_line(&entry, 40);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.starts_with("+3 -0 "), "stat prefix: {text:?}");
    assert!(text.contains("a.rs"));
    assert!(
        line.spans.iter().all(|s| s.style.fg == Some(Color::Green)),
        "added file rendered green"
    );
}

#[test]
fn git_file_line_deleted_is_red_and_modified_is_blue() {
    let deleted = GitFileEntry {
        path: "d.rs".to_string(),
        change_type: GitFileChangeType::Deleted,
        additions: 0,
        deletions: 4,
        binary: false,
    };
    let modified = GitFileEntry {
        path: "m.rs".to_string(),
        change_type: GitFileChangeType::Modified,
        additions: 1,
        deletions: 1,
        binary: false,
    };
    assert_eq!(
        git_file_line(&deleted, 40).spans[0].style.fg,
        Some(Color::Red)
    );
    assert_eq!(
        git_file_line(&modified, 40).spans[0].style.fg,
        Some(Color::Blue)
    );
}

#[test]
fn git_file_line_binary_has_suffix() {
    let entry = GitFileEntry {
        path: "img.png".to_string(),
        change_type: GitFileChangeType::Added,
        additions: 0,
        deletions: 0,
        binary: true,
    };
    let line = git_file_line(&entry, 40);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("(binary)"), "binary suffix: {text:?}");
}

// ── truncate_path ──────────────────────────────────────────────────────

#[test]
fn truncate_path_short_unchanged() {
    assert_eq!(truncate_path("src/foo.rs", 40), "src/foo.rs");
}

#[test]
fn truncate_path_long_gets_ellipsis() {
    let p = "src/deeply/nested/very-long-file-name.rs";
    let out = truncate_path(p, 10);
    assert!(out.ends_with('\u{2026}'), "truncated with …: {out:?}");
    assert_eq!(out.chars().count(), 10);
}

#[test]
fn truncate_path_zero_width_is_empty() {
    assert_eq!(truncate_path("anything", 0), "");
}

#[test]
fn truncate_path_at_exact_width_unchanged() {
    assert_eq!(truncate_path("abcdef", 6), "abcdef");
}

// ── render_config_show: truncation + detail line (WI-0095) ───────────────

#[test]
fn config_show_truncates_long_cell_and_shows_full_value_in_detail_line() {
    let field = "dynamicWorkflows.agentsToModels.claude".to_string();
    let value = "claude-opus-4-8, claude-sonnet-4-6, gemini-2.5-pro".to_string();
    let state = dialogs::ConfigShowState {
        rows: vec![dialogs::ConfigShowRow {
            field: field.clone(),
            global: String::new(),
            repo: value.clone(),
            effective: value.clone(),
            read_only: false,
            global_writable: false,
            repo_writable: true,
            value_hint: None,
        }],
        selected: 0,
        editing: false,
        edit_column: 0,
        editor: crate::frontend::tui::text_edit::TextEdit::new(false),
        error: None,
        new_entry: None,
    };

    let buf = render_to_buffer(140, 30, |area, frame| {
        render_config_show(&state, area, frame);
    });
    let text = buffer_text(&buf);
    let lines: Vec<&str> = text.lines().collect();

    let header_idx = lines
        .iter()
        .position(|l| l.contains("Field") && l.contains("Effective"))
        .expect("header row must be present");
    let data_row = lines[header_idx + 1];

    assert!(
        data_row.contains('\u{2026}'),
        "long value must be truncated with an ellipsis in the table cell: {data_row:?}"
    );
    assert!(
        !data_row.contains(&value),
        "the full untruncated value must not fit in the table row cell: {data_row:?}"
    );
    assert!(
        text.contains(&value),
        "the full untruncated value must appear in the detail line when the row is \
         focused: {text}"
    );
}

#[test]
fn config_show_scrolls_to_reveal_rows_past_the_visible_window() {
    // Reproduces the bug where dynamicWorkflows.agentsToModels rows (WI-0095),
    // appended after ~20 other config rows, fell past the visible table
    // height and were never rendered — the Table widget always started
    // from row 0 with no scroll offset tied to `selected`.
    fn build_rows(target_field: &str) -> Vec<dialogs::ConfigShowRow> {
        let mut rows: Vec<dialogs::ConfigShowRow> = (0..20)
            .map(|i| dialogs::ConfigShowRow {
                field: format!("field{i}"),
                global: String::new(),
                repo: "x".to_string(),
                effective: "x".to_string(),
                read_only: false,
                global_writable: true,
                repo_writable: true,
                value_hint: None,
            })
            .collect();
        rows.push(dialogs::ConfigShowRow {
            field: target_field.to_string(),
            global: String::new(),
            repo: "claude-opus-4-8, claude-sonnet-4-6".to_string(),
            effective: "claude-opus-4-8, claude-sonnet-4-6".to_string(),
            read_only: false,
            global_writable: false,
            repo_writable: true,
            value_hint: None,
        });
        rows
    }
    let target_field = "dynamicWorkflows.agentsToModels.claude".to_string();
    let last_index = build_rows(&target_field).len() - 1;

    // Short terminal: fewer visible table rows than total rows, so the
    // last row is off-screen until the selection scrolls down to it.
    let state_unselected = dialogs::ConfigShowState {
        rows: build_rows(&target_field),
        selected: 0,
        editing: false,
        edit_column: 0,
        editor: crate::frontend::tui::text_edit::TextEdit::new(false),
        error: None,
        new_entry: None,
    };
    let buf = render_to_buffer(140, 20, |area, frame| {
        render_config_show(&state_unselected, area, frame);
    });
    assert!(
        !buffer_text(&buf).contains(&target_field),
        "row should not be visible before scrolling to it"
    );

    let state_selected = dialogs::ConfigShowState {
        rows: build_rows(&target_field),
        selected: last_index,
        editing: false,
        edit_column: 0,
        editor: crate::frontend::tui::text_edit::TextEdit::new(false),
        error: None,
        new_entry: None,
    };
    let buf = render_to_buffer(140, 20, |area, frame| {
        render_config_show(&state_selected, area, frame);
    });
    assert!(
        buffer_text(&buf).contains(&target_field),
        "selecting the last row must scroll it into view"
    );
}

#[test]
fn config_show_popup_takes_90_percent_of_the_terminal() {
    let state = dialogs::ConfigShowState {
        rows: vec![],
        selected: 0,
        editing: false,
        edit_column: 0,
        editor: crate::frontend::tui::text_edit::TextEdit::new(false),
        error: None,
        new_entry: None,
    };
    let buf = render_to_buffer(100, 40, |area, frame| {
        render_config_show(&state, area, frame);
    });
    // 90% of 100x40 centered → the border starts near x=5, y=2 and the
    // popup spans ~90 columns. Locate the rounded corner glyphs.
    let text = buffer_text(&buf);
    let top_line_idx = text
        .lines()
        .position(|l| l.contains('\u{256d}'))
        .expect("popup top border must be present");
    let top_line = text.lines().nth(top_line_idx).unwrap();
    let left = top_line.find('\u{256d}').unwrap();
    let right = top_line.rfind('\u{256e}').unwrap();
    assert!(
        top_line_idx <= 3,
        "popup must start near the top (90% height), got row {top_line_idx}"
    );
    assert!(
        right - left >= 85,
        "popup must span ~90% of the width, got {} cols",
        right - left
    );
}

#[test]
fn config_show_editing_long_value_keeps_cursor_visible_in_cell() {
    let long = "a".repeat(120);
    let mut editor = crate::frontend::tui::text_edit::TextEdit::new(false);
    editor.set_text(&long); // cursor at the end
    let state = dialogs::ConfigShowState {
        rows: vec![dialogs::ConfigShowRow {
            field: "overlays".into(),
            global: long.clone(),
            repo: String::new(),
            effective: long.clone(),
            read_only: false,
            global_writable: true,
            repo_writable: true,
            value_hint: None,
        }],
        selected: 0,
        editing: true,
        edit_column: 0,
        editor,
        error: None,
        new_entry: None,
    };
    let buf = render_to_buffer(120, 30, |area, frame| {
        render_config_show(&state, area, frame);
    });
    let text = buffer_text(&buf);
    assert!(
        text.contains("a|"),
        "the cell must window the value around the cursor so it stays visible:\n{text}"
    );
}

#[test]
fn config_show_add_mapping_prompts_render_per_phase() {
    let mut editor = crate::frontend::tui::text_edit::TextEdit::new(false);
    editor.set_text("maki");
    let key_phase = dialogs::ConfigShowState {
        rows: vec![],
        selected: 0,
        editing: true,
        edit_column: 1,
        editor,
        error: None,
        new_entry: Some(dialogs::NewMapEntryPhase::Key),
    };
    let text = buffer_text(&render_to_buffer(120, 30, |area, frame| {
        render_config_show(&key_phase, area, frame);
    }));
    assert!(
        text.contains("agent name: maki|"),
        "key phase must prompt for the agent name with a cursor:\n{text}"
    );

    let mut editor = crate::frontend::tui::text_edit::TextEdit::new(false);
    editor.set_text("model-a");
    let value_phase = dialogs::ConfigShowState {
        rows: vec![],
        selected: 0,
        editing: true,
        edit_column: 1,
        editor,
        error: None,
        new_entry: Some(dialogs::NewMapEntryPhase::Value { key: "maki".into() }),
    };
    let text = buffer_text(&render_to_buffer(120, 30, |area, frame| {
        render_config_show(&value_phase, area, frame);
    }));
    assert!(
        text.contains("dynamicWorkflows.agentsToModels.maki = model-a|"),
        "value phase must show the pending field and model list:\n{text}"
    );
}

#[test]
fn config_show_ctrl_n_hint_only_on_agents_to_models_rows() {
    fn browse_state(field: &str) -> dialogs::ConfigShowState {
        dialogs::ConfigShowState {
            rows: vec![dialogs::ConfigShowRow {
                field: field.to_string(),
                global: String::new(),
                repo: "x".to_string(),
                effective: "x".to_string(),
                read_only: false,
                global_writable: false,
                repo_writable: true,
                value_hint: None,
            }],
            selected: 0,
            editing: false,
            edit_column: 0,
            editor: crate::frontend::tui::text_edit::TextEdit::new(false),
            error: None,
            new_entry: None,
        }
    }

    let unrelated = buffer_text(&render_to_buffer(140, 30, |area, frame| {
        render_config_show(&browse_state("agent"), area, frame);
    }));
    assert!(
        !unrelated.contains("Ctrl+N"),
        "the add-mapping hint must not show on rows unrelated to \
         agentsToModels:\n{unrelated}"
    );

    for field in [
        "dynamicWorkflows.agentsToModels",
        "dynamicWorkflows.agentsToModels.claude",
    ] {
        let mapping = buffer_text(&render_to_buffer(140, 30, |area, frame| {
            render_config_show(&browse_state(field), area, frame);
        }));
        assert!(
            mapping.contains("Ctrl+N"),
            "the add-mapping hint must show on {field}:\n{mapping}"
        );
    }
}

#[test]
fn config_show_renders_rejection_reason_over_format_hint() {
    let mut editor = crate::frontend::tui::text_edit::TextEdit::new(false);
    editor.set_text("claude");
    let state = dialogs::ConfigShowState {
        rows: vec![dialogs::ConfigShowRow {
            field: "dynamicWorkflows.defaultLeader".to_string(),
            global: String::new(),
            repo: String::new(),
            effective: String::new(),
            read_only: false,
            global_writable: false,
            repo_writable: true,
            value_hint: Some("agent::model".to_string()),
        }],
        selected: 0,
        editing: true,
        edit_column: 1,
        editor,
        error: Some("'claude' is not a valid leader; expected agent::model".to_string()),
        new_entry: None,
    };
    let text = buffer_text(&render_to_buffer(140, 30, |area, frame| {
        render_config_show(&state, area, frame);
    }));
    assert!(
        text.contains("✗ 'claude' is not a valid leader; expected agent::model"),
        "the rejection reason must be visible in the dialog:\n{text}"
    );
    assert!(
        text.contains("claude|"),
        "the rejected input must be preserved in the editor:\n{text}"
    );
}

// ── cursor_window ─────────────────────────────────────────────────────────

#[test]
fn cursor_window_short_text_is_untouched_with_cursor_inserted() {
    assert_eq!(cursor_window("abc", 1, 10), "a|bc");
}

#[test]
fn cursor_window_clips_left_to_keep_cursor_visible() {
    let text = "abcdefghij";
    let out = cursor_window(text, text.len(), 6);
    assert!(
        out.ends_with("ij|"),
        "cursor at the end must stay visible: {out:?}"
    );
    assert!(out.starts_with('\u{2026}'), "clipped left marked: {out:?}");
    assert_eq!(out.chars().count(), 6);
}

#[test]
fn cursor_window_clips_right_when_cursor_at_start() {
    let out = cursor_window("abcdefghij", 0, 6);
    assert!(out.starts_with("|abcd"), "cursor first: {out:?}");
    assert!(out.ends_with('\u{2026}'), "clipped right marked: {out:?}");
    assert_eq!(out.chars().count(), 6);
}

#[test]
fn cursor_window_handles_multibyte_text() {
    let text = "héllo wörld";
    // Cursor after "héllo" (byte index of the space).
    let cursor = text.find(' ').unwrap();
    let out = cursor_window(text, cursor, 8);
    assert!(out.contains('|'), "cursor must be present: {out:?}");
    assert!(out.chars().count() <= 8);
}

#[test]
fn capture_buffer_grid_reads_cell_symbols() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 10, 5));
    buf.set_string(2, 1, "abcd", Style::default());
    buf.set_string(2, 2, "ef", Style::default());

    let grid = capture_buffer_grid(&buf, Rect::new(2, 1, 4, 2));
    assert_eq!(grid.len(), 2);
    assert_eq!(grid[0], vec!["a", "b", "c", "d"]);
    assert_eq!(grid[1], vec!["e", "f", " ", " "], "empties become spaces");
}

#[test]
fn apply_selection_highlight_reverses_selected_cells_only() {
    let area = Rect::new(2, 1, 4, 2);
    let mut buf = Buffer::empty(Rect::new(0, 0, 10, 5));
    // Cells (1,0)..=(2,0) relative to area → screen (3,1)..=(4,1).
    let sel = TextSelection {
        start_col: 1,
        start_row: 0,
        end_col: 2,
        end_row: 0,
        snapshot: Vec::new(),
    };
    apply_selection_highlight(&mut buf, area, &sel);

    let reversed = |x: u16, y: u16| {
        buf.cell((x, y))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED)
    };
    assert!(reversed(3, 1));
    assert!(reversed(4, 1));
    assert!(!reversed(2, 1), "cell before selection start");
    assert!(!reversed(5, 1), "cell after selection end");
    assert!(!reversed(3, 2), "row below selection");
}

#[test]
fn apply_selection_highlight_normalizes_backward_drag() {
    let area = Rect::new(0, 0, 6, 3);
    let mut buf = Buffer::empty(Rect::new(0, 0, 6, 3));
    // Dragged up-left: start after end.
    let sel = TextSelection {
        start_col: 2,
        start_row: 1,
        end_col: 4,
        end_row: 0,
        snapshot: Vec::new(),
    };
    apply_selection_highlight(&mut buf, area, &sel);

    let reversed = |x: u16, y: u16| {
        buf.cell((x, y))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED)
    };
    assert!(reversed(4, 0), "normalized start cell");
    assert!(reversed(2, 1), "normalized end cell");
    assert!(!reversed(3, 0), "cell before normalized start");
    assert!(!reversed(3, 1), "cell after normalized end");
}

#[test]
fn long_path_truncated_with_middle_ellipsis() {
    let long_path = "/home/user/projects/very-long-directory-name/another-long-part/file.txt";
    let result = truncate_middle(long_path, 30);
    assert!(
        result.contains('\u{2026}'),
        "long path must be truncated with '…', got: {result:?}"
    );
    assert!(
        result.chars().count() <= 30,
        "truncated string must be at most 30 chars, got {} chars: {result:?}",
        result.chars().count()
    );
}

#[test]
fn short_path_not_truncated() {
    let short = "/home/user/foo";
    let result = truncate_middle(short, 40);
    assert_eq!(result, short, "path shorter than max must not be truncated");
}

#[test]
fn truncate_middle_exact_length_not_truncated() {
    let s = "abcdefghij"; // 10 chars
    let result = truncate_middle(s, 10);
    assert_eq!(
        result, s,
        "string at exactly max chars must not be truncated"
    );
}

#[test]
fn truncate_middle_preserves_prefix_and_suffix() {
    let s = "start-middle-end";
    let result = truncate_middle(s, 10);
    assert!(result.starts_with("star"), "prefix must be preserved");
    assert!(result.ends_with("end"), "suffix must be preserved");
    assert!(result.contains('\u{2026}'));
}

#[test]
fn command_box_scroll_offset_no_scroll_when_cursor_fits() {
    assert_eq!(command_box_scroll_offset(0, 80), 0);
    assert_eq!(command_box_scroll_offset(79, 80), 0);
}

#[test]
fn command_box_scroll_offset_scrolls_long_input() {
    // Cursor at the width boundary scrolls one column off the left.
    assert_eq!(command_box_scroll_offset(80, 80), 1);
    assert_eq!(command_box_scroll_offset(100, 80), 21);
    // Cursor stays at the last visible column in both cases.
    assert_eq!(80 - command_box_scroll_offset(80, 80), 79);
    assert_eq!(100 - command_box_scroll_offset(100, 80), 79);
}

#[test]
fn command_box_scroll_offset_zero_width_does_not_underflow() {
    // Degenerate terminal (width 0): everything scrolls off; the cursor
    // math downstream saturates to column 0 instead of panicking.
    assert_eq!(command_box_scroll_offset(0, 0), 1);
    assert_eq!(command_box_scroll_offset(5, 0), 6);
    assert_eq!(5usize.saturating_sub(command_box_scroll_offset(5, 0)), 0);
}
