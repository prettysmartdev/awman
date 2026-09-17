//! Squad tab body. Sibling of `command_box.rs` / `tab_bar.rs`.
//!
//! Renders the task grid plus the key-hint line. Reads only `Tab::squad`'s
//! shared snapshot — never the tab's synthetic `Session`, never the persistent
//! task store, never a gateway.

use super::*;

use crate::data::fs::task_store::{RunStatus, Task, TaskStatus};
use crate::frontend::tui::tabs::squad_state::SquadSnapshot;

/// Minimum card size, in cells, per WI 0106 Part 5 ("generous size and
/// spacing"). The grid reflows its column count to keep every card at least
/// this wide as the tab is resized; card height is [`grid_card_height`].
const CARD_MIN_WIDTH: u16 = 30;
/// Two border rows plus the five body rows a card always carries: the
/// `Description` label, the description itself, and the `Last run` / `Outcome`
/// / `Next` pairs. The description gets a whole row of its own rather than
/// sharing one with its label, because on a minimum-width card an inline
/// `Description: ` would leave under half the row for the text it introduces.
const CARD_MIN_HEIGHT: u16 = 7;
/// The extra body row a card needs for WI 0116 §6b's `Env` line. Applied to
/// *every* card in the grid, but only when some task in the grid actually has
/// an unmet variable: cards must stay a uniform height (they are laid out on
/// shared rows), and a permanently taller card would cost every user a row of
/// vertical space to carry a flag almost none of them ever raise.
const CARD_ENV_ROW_HEIGHT: u16 = 1;
const CARD_COL_SPACING: u16 = 2;
const CARD_ROW_SPACING: u16 = 1;

/// The most columns the card grid ever lays out (WI 0110). This is what makes
/// a card at least a *third* of the grid width: `CARD_MIN_WIDTH` alone let a
/// wide terminal produce five or six columns of barely-readable cards, each
/// clipping its description to 30 cells. Fewer tasks still render fewer
/// columns — this is a ceiling, not a target.
const MAX_CARD_COLUMNS: usize = 3;

/// Render the squad task list into `area`.
pub(super) fn render_squad_body(app: &mut App, area: Rect, frame: &mut Frame) {
    let (snapshot, selected) = {
        let tab = app.active_tab();
        match tab.squad.as_ref() {
            Some(state) => {
                let snap = state
                    .snapshot
                    .lock()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                (snap, state.selected)
            }
            None => (SquadSnapshot::default(), 0),
        }
    };

    let block = Block::default()
        .title(" squad ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Optional header: the daemon-down state (kept above the last-known list,
    // never replacing it with an empty one), or the pre-first-poll loading
    // line.
    //
    // A *failed squad action* is deliberately not here: it is rendered in the
    // hint bar above the command box (`render/status_bar.rs`), the one row
    // that is on screen for every tab and already the place the TUI reports
    // what just happened. Keeping it out of the grid header stops the cards
    // from shifting down a row every time an action fails.
    let mut header_lines: Vec<Line> = Vec::new();
    if let Some(msg) = snapshot.error.as_ref() {
        header_lines.push(Line::from(Span::styled(
            "squad daemon not reachable",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        header_lines.push(Line::from(Span::styled(
            msg.clone(),
            Style::default().fg(Color::Red),
        )));
    } else if !snapshot.loaded {
        header_lines.push(Line::from(Span::styled(
            "Loading\u{2026}",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let header_h = header_lines.len() as u16;

    let hint = "enter detail \u{b7} h history \u{b7} a attach \u{b7} n new \u{b7} e edit \u{b7} \
                t trigger \u{b7} p pause \u{b7} r resume \u{b7} d delete";

    let chunks = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    if header_h > 0 {
        frame.render_widget(Paragraph::new(header_lines), chunks[0]);
    }

    let columns = render_task_grid(&snapshot.tasks, selected, chunks[1], frame);
    // Publish the column count the grid was actually laid out with so
    // Left/Right/Up/Down (`SquadTabState::move_selection`/`move_selection_col`)
    // can turn the linear `selected` index into 2D movement. Selection itself
    // stays a linear index into `tasks` (never a `(row, col)` pair), so a
    // reflow that changes `columns` can never silently reselect a different
    // task — see the field doc on `SquadTabState::grid_columns`.
    if let Some(state) = app.active_tab_mut().squad.as_mut() {
        state.grid_columns = columns;
    }

    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        chunks[2],
    );
}

/// The column count a grid of `CARD_MIN_WIDTH`-plus-spacing cards fits into
/// `width`, capped at [`MAX_CARD_COLUMNS`]. Always at least 1, even when
/// `width` is narrower than one card — the single column just renders squeezed
/// rather than disappearing.
fn grid_columns_for_width(width: u16) -> usize {
    if width == 0 {
        return 1;
    }
    let cell = CARD_MIN_WIDTH + CARD_COL_SPACING;
    ((width / cell) as usize).clamp(1, MAX_CARD_COLUMNS)
}

/// Lay `tasks` out as a grid of rounded-rectangle cards and render it into
/// `area`. Returns the column count used, so the caller can publish it for
/// the key handler's 2D navigation. Renders an empty-state message instead
/// of a zero-card grid when `tasks` is empty.
fn render_task_grid(tasks: &[Task], selected: usize, area: Rect, frame: &mut Frame) -> usize {
    if tasks.is_empty() {
        render_empty_state(area, frame);
        return 1;
    }

    let columns = grid_columns_for_width(area.width);
    let card_height = grid_card_height(tasks);
    let row_cell = card_height + CARD_ROW_SPACING;
    let visible_rows = ((area.height / row_cell) as usize).max(1);
    let rows_total = tasks.len().div_ceil(columns);

    // When every row fits, show them all; otherwise scroll the row window so
    // the selected card is always on screen (selection is a linear index, so
    // this window never changes what task is selected — only what's drawn).
    let selected_row = selected / columns;
    let start_row = if rows_total <= visible_rows {
        0
    } else {
        selected_row
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(rows_total - visible_rows)
    };
    let end_row = (start_row + visible_rows).min(rows_total);

    let row_constraints: Vec<Constraint> = (start_row..end_row)
        .map(|_| Constraint::Length(card_height))
        .collect();
    let row_areas = Layout::vertical(row_constraints)
        .spacing(CARD_ROW_SPACING)
        .split(area);

    // A card never spans more than half the grid width: with few columns the
    // remaining width is left empty rather than stretching one card across the
    // whole tab. On a terminal too narrow for a half-width card to reach the
    // minimum card width, the minimum wins and the card may exceed half.
    let card_width = grid_card_width(area.width, columns);

    for (ri, row_area) in row_areas.iter().enumerate() {
        let row = start_row + ri;
        let row_start = row * columns;
        let row_end = (row_start + columns).min(tasks.len());
        let n = row_end - row_start;
        if n == 0 {
            continue;
        }
        let col_constraints: Vec<Constraint> =
            (0..n).map(|_| Constraint::Length(card_width)).collect();
        // Flex::Start pins every card to its fixed width: leftover row width
        // stays empty instead of stretching the final card back to full width.
        let col_areas = Layout::horizontal(col_constraints)
            .flex(ratatui::layout::Flex::Start)
            .spacing(CARD_COL_SPACING)
            .split(*row_area);
        for (ci, card_area) in col_areas.iter().enumerate() {
            let idx = row_start + ci;
            render_task_card(&tasks[idx], idx == selected, *card_area, frame);
        }
    }

    columns
}

/// The height every card in the grid renders at: [`CARD_MIN_HEIGHT`], plus one
/// row when any task in `tasks` carries an unmet `env()` name.
///
/// Measured across the whole grid rather than per card because cards share
/// layout rows — a taller card in one column would leave a ragged row — and
/// because a card that grew only when affected would shift its neighbours
/// every time the daemon's coverage changed. With no unmet variable anywhere,
/// which is the common case, the grid is laid out exactly as it was before
/// WI 0116.
fn grid_card_height(tasks: &[Task]) -> u16 {
    if tasks.iter().any(|task| !task.unmet_env.is_empty()) {
        CARD_MIN_HEIGHT + CARD_ENV_ROW_HEIGHT
    } else {
        CARD_MIN_HEIGHT
    }
}

/// The width every card in the grid renders at: an even share of the grid
/// width, floored at a third of it (WI 0110) and capped at half of it (WI
/// 0108: a lone column must not produce a full-width card).
/// `CARD_MIN_WIDTH` still wins over the half-width cap so narrow terminals
/// keep a readable card.
///
/// The floor is what [`MAX_CARD_COLUMNS`] buys: with at most three columns the
/// even share is already at least a third, so the two agree by construction
/// rather than by coincidence — the floor is stated here so a future column
/// change cannot silently reintroduce sliver cards.
fn grid_card_width(area_width: u16, columns: usize) -> u16 {
    let columns = (columns.max(1)) as u16;
    let total_spacing = CARD_COL_SPACING * columns.saturating_sub(1);
    let usable = area_width.saturating_sub(total_spacing);
    let share = usable / columns;
    let floor = usable / MAX_CARD_COLUMNS as u16;
    let cap = (area_width / 2).max(CARD_MIN_WIDTH.min(area_width));
    share.min(cap).max(floor).max(1)
}

/// A message in place of the grid when there are no tasks — never a
/// zero-card layout with dangling borders.
fn render_empty_state(area: Rect, frame: &mut Frame) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let line_area = Rect {
        x: area.x,
        y: area.y + area.height / 2,
        width: area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            "No squad tasks yet \u{2014} press 'n' to create one.",
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Center),
        line_area,
    );
}

/// What a task card's *colour* says about the task (WI 0112 Part 3). One
/// axis only — selection is the other axis (dashed vs solid outline, the
/// `➡` marker) and never expresses state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardStatus {
    /// `TaskStatus::Paused`. The user switched it off; outranks everything.
    Paused,
    /// The most recent run is still going.
    Running,
    /// A `squad trigger` is waiting to be honoured on the next tick.
    Triggered,
    /// The most recent run ended `failed`.
    Failed,
    /// Has never run.
    NeverRun,
    /// Active, with an ordinary last outcome (or interrupted).
    Active,
}

impl CardStatus {
    pub(crate) fn color(self) -> Color {
        match self {
            Self::Paused => Color::DarkGray,
            Self::Running => Color::Blue,
            Self::Triggered => Color::Magenta,
            Self::Failed => Color::Red,
            Self::NeverRun => Color::Yellow,
            Self::Active => Color::Green,
        }
    }
}

/// The card colour for `task`, first match wins. Paused is a user decision
/// and outranks everything (the card should read "you switched this off"
/// even if its last run failed). A running task cannot honour a trigger
/// until it finishes, so running outranks triggered. Triggered outranks
/// failed because the user has just acted on the task and wants to see the
/// trigger acknowledged; the red returns if the triggered run fails too.
pub(crate) fn card_status(task: &Task) -> CardStatus {
    if task.status == TaskStatus::Paused {
        return CardStatus::Paused;
    }
    if task.last_run_status == Some(RunStatus::Running) {
        return CardStatus::Running;
    }
    if task.trigger_requested_at.is_some() {
        return CardStatus::Triggered;
    }
    if task.last_run_status == Some(RunStatus::Failed) {
        return CardStatus::Failed;
    }
    if task.last_run_at.is_none() {
        return CardStatus::NeverRun;
    }
    CardStatus::Active
}

/// The outline every *unselected* card is drawn with: rounded corners with
/// dashed edges. Ratatui has no dashed `BorderType`, so it is a custom set.
/// The selected card uses the ordinary solid `BorderType::Rounded`; dashed
/// versus solid means only "not selected" versus "selected".
pub(crate) const DASHED_ROUNDED: ratatui::symbols::border::Set = ratatui::symbols::border::Set {
    top_left: "\u{256d}",
    top_right: "\u{256e}",
    bottom_left: "\u{2570}",
    bottom_right: "\u{256f}",
    vertical_left: "\u{2506}",
    vertical_right: "\u{2506}",
    horizontal_top: "\u{254c}",
    horizontal_bottom: "\u{254c}",
};

/// Render a single task as a rounded-rectangle card: name as the block
/// title, then the same three fields the table used to show as columns
/// (summary, last run, next evaluation) as body lines.
///
/// Colour comes from [`card_status`]; selection is a solid (not dashed)
/// outline plus the same `➡` title marker the active tab carries.
fn render_task_card(task: &Task, is_selected: bool, area: Rect, frame: &mut Frame) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let color = card_status(task).color();
    let border_style = Style::default().fg(color);
    let (title_text, title_style) = if is_selected {
        (
            format!(" \u{27a1} {} ", task.name),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            format!(" {} ", task.name),
            Style::default().add_modifier(Modifier::BOLD),
        )
    };
    let block = Block::default()
        .title(Span::styled(title_text, title_style))
        .borders(Borders::ALL)
        .border_style(border_style);
    let block = if is_selected {
        block.border_type(BorderType::Rounded)
    } else {
        block.border_set(DASHED_ROUNDED)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let last_run = task
        .last_run_at
        .map(format_time)
        .unwrap_or_else(|| "\u{2014}".to_string());
    let next = next_evaluation(task);

    // Every value carries a grey label, so a card reads as a labelled record
    // rather than a stack of bare values whose meaning has to be inferred from
    // their order. The description used to be an unlabelled first line and the
    // last-run *timestamp* an unlabelled continuation of the outcome line;
    // both now say what they are. The description's label sits on its own row
    // so the text keeps the card's full width (the detail modal lays it out
    // the same way, for the same reason).
    //
    // No `.wrap()`: each `Line` is horizontally clipped to `inner.width` by
    // the buffer. Values are truncated explicitly (with an ellipsis) to the
    // width their label leaves, so the cut is visible rather than a silent
    // clip.
    let mut lines = vec![
        Line::from(Span::styled(
            "Description",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::raw(truncate_to_width(
            &first_line(&task.description),
            inner.width as usize,
        ))),
        labelled_card_line("Last run", &last_run, inner.width),
        labelled_card_line("Outcome", last_run_outcome(task), inner.width),
        labelled_card_line("Next", &next, inner.width),
    ];
    // WI 0116 §6b: one more labelled row when the daemon has no value for one
    // of this task's `env()` names, and no row at all otherwise.
    //
    // Deliberately NOT a `CardStatus` variant. That precedence table answers
    // "what is this task's run state" and is documented and tested as such; an
    // unmet variable is orthogonal — a *paused* task can have one too — so
    // overloading the card colour would make two unrelated facts compete for
    // one channel. A labelled row states the fact without taking the colour.
    if !task.unmet_env.is_empty() {
        lines.push(labelled_card_line(
            "Env",
            &format!("\u{26a0} {} unmet", task.unmet_env.join(", ")),
            inner.width,
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One `label: value` card body line: the label in grey, the value in the
/// default foreground, truncated to whatever width the label leaves.
///
/// A card can be as narrow as [`CARD_MIN_WIDTH`], so the value is measured
/// against `width` minus the label rather than against the whole card — a
/// value truncated to the full card width would still overflow the border by
/// the length of its own label.
fn labelled_card_line<'a>(label: &str, value: &str, width: u16) -> Line<'a> {
    let label = format!("{label}: ");
    let value_width =
        (width as usize).saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()));
    Line::from(vec![
        Span::styled(label, Style::default().fg(Color::DarkGray)),
        Span::raw(truncate_to_width(value, value_width)),
    ])
}

/// The outcome of the task's most recent run — what actually happened, not
/// whether the task is scheduled. Paused/active is separate state and is shown
/// separately; a task that has never run reads `never run`.
fn last_run_outcome(task: &Task) -> &'static str {
    match task.last_run_status {
        Some(RunStatus::Running) => "running",
        Some(RunStatus::NotTriggered) => "not triggered",
        Some(RunStatus::WorkflowExecuted) => "workflow executed",
        Some(RunStatus::Failed) => "failed",
        Some(RunStatus::Interrupted) => "interrupted",
        None => "never run",
    }
}

/// The first line of a (possibly multi-line) description, used as the
/// card's short summary.
fn first_line(description: &str) -> String {
    description.lines().next().unwrap_or("").to_string()
}

/// Truncate `text` to at most `width` display cells, ending in an ellipsis
/// when anything was cut. Width-aware so wide characters never overflow the
/// card border.
fn truncate_to_width(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    use unicode_width::UnicodeWidthStr;

    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1); // reserve one cell for the ellipsis
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// Format a timestamp for the card fields.
fn format_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

/// The task's next scheduled evaluation, as display text:
/// `paused` when paused, `now` when it has never run, otherwise
/// `last_run_at + interval`, or `backoff_until` when that is later.
fn next_evaluation(task: &Task) -> String {
    if task.status == TaskStatus::Paused {
        return "paused".to_string();
    }
    // A pending `t` outranks the schedule, so the card says so: the whole
    // point of triggering is to see that the task is no longer waiting for its
    // interval, and a card still showing "2026-09-03 04:00" would read as a
    // key that did nothing.
    if task.trigger_requested_at.is_some() {
        return "triggered \u{2014} next tick".to_string();
    }
    let Some(last) = task.last_run_at else {
        return "now".to_string();
    };
    let mut next = last + chrono::Duration::seconds(task.interval_secs as i64);
    if let Some(backoff) = task.backoff_until {
        if backoff > next {
            next = backoff;
        }
    }
    format_time(next)
}

#[cfg(test)]
mod tests {
    use super::{
        card_status, grid_card_width, grid_columns_for_width, truncate_to_width, CardStatus,
        CARD_MIN_WIDTH, MAX_CARD_COLUMNS,
    };
    use crate::data::fs::task_store::{MountScope, RunStatus, Task, TaskStatus};
    use chrono::Utc;

    fn task_with(
        status: TaskStatus,
        last_run_status: Option<RunStatus>,
        has_run: bool,
        triggered: bool,
    ) -> Task {
        let now = Utc::now();
        Task {
            id: "t".into(),
            name: "t".into(),
            description: "d".into(),
            repo_scope: std::path::PathBuf::from("/workspace"),
            mount_scope: MountScope::Directory,
            overlays: Vec::new(),
            interval_secs: 60,
            status,
            agent: None,
            model: None,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: has_run.then_some(now),
            trigger_requested_at: triggered.then_some(now),
            last_run_status,
            unmet_env: Vec::new(),
        }
    }

    /// WI 0112 Part 3: every row of the precedence table.
    #[test]
    fn card_status_follows_the_precedence_table() {
        use TaskStatus::{Active, Paused};
        assert_eq!(
            card_status(&task_with(Paused, None, false, false)),
            CardStatus::Paused
        );
        assert_eq!(
            card_status(&task_with(Active, Some(RunStatus::Running), true, false)),
            CardStatus::Running
        );
        assert_eq!(
            card_status(&task_with(
                Active,
                Some(RunStatus::WorkflowExecuted),
                true,
                true
            )),
            CardStatus::Triggered
        );
        assert_eq!(
            card_status(&task_with(Active, Some(RunStatus::Failed), true, false)),
            CardStatus::Failed
        );
        assert_eq!(
            card_status(&task_with(Active, None, false, false)),
            CardStatus::NeverRun
        );
        for outcome in [
            RunStatus::NotTriggered,
            RunStatus::WorkflowExecuted,
            RunStatus::Interrupted,
        ] {
            assert_eq!(
                card_status(&task_with(Active, Some(outcome), true, false)),
                CardStatus::Active,
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn card_status_combinations_resolve_in_order() {
        use TaskStatus::{Active, Paused};
        // Paused beats failed: the user switched it off.
        assert_eq!(
            card_status(&task_with(Paused, Some(RunStatus::Failed), true, false)),
            CardStatus::Paused
        );
        // Running beats triggered: the trigger waits for the run to end.
        assert_eq!(
            card_status(&task_with(Active, Some(RunStatus::Running), true, true)),
            CardStatus::Running
        );
        // Triggered beats failed: the trigger is acknowledged first.
        assert_eq!(
            card_status(&task_with(Active, Some(RunStatus::Failed), true, true)),
            CardStatus::Triggered
        );
        // A never-run task that is triggered is triggered, not never-run.
        assert_eq!(
            card_status(&task_with(Active, None, false, true)),
            CardStatus::Triggered
        );
    }

    #[test]
    fn a_single_column_card_is_capped_at_half_the_grid_width() {
        assert_eq!(
            grid_card_width(120, 1),
            60,
            "one column must not span the tab"
        );
        // Two columns already share the width below the cap.
        assert_eq!(grid_card_width(120, 2), 59);
    }

    /// WI 0110: a card is never narrower than a third of the grid, which is
    /// what capping the grid at three columns buys. Without the cap a wide
    /// terminal produced six or more `CARD_MIN_WIDTH` slivers.
    #[test]
    fn the_grid_never_lays_out_more_than_three_columns() {
        // 30 + 2 spacing per cell: 240 cells would fit seven columns.
        assert_eq!(grid_columns_for_width(240), MAX_CARD_COLUMNS);
        assert_eq!(grid_columns_for_width(1000), MAX_CARD_COLUMNS);
        // Below the cap the count still follows the width.
        assert_eq!(grid_columns_for_width(100), 3);
        assert_eq!(grid_columns_for_width(70), 2);
        assert_eq!(grid_columns_for_width(40), 1);
        assert_eq!(
            grid_columns_for_width(0),
            1,
            "a zero-width grid still has a column"
        );
    }

    /// A third of the *layout* width — the grid minus inter-card spacing —
    /// because the gutters have to come out of somewhere, and a floor measured
    /// against the raw grid width could not be satisfied by three cards plus
    /// two gutters.
    #[test]
    fn every_card_is_at_least_a_third_of_the_grid_width() {
        for width in [40u16, 96, 120, 240, 400] {
            let columns = grid_columns_for_width(width);
            let card = grid_card_width(width, columns);
            let spacing = super::CARD_COL_SPACING * (columns as u16 - 1);
            let third = width.saturating_sub(spacing) / MAX_CARD_COLUMNS as u16;
            assert!(
                card >= third,
                "a {width}-wide grid laid out {columns} columns of {card}-wide cards, \
                 below a third ({third})"
            );
            assert!(
                card * columns as u16 + spacing <= width || card == CARD_MIN_WIDTH,
                "{columns} cards of {card} plus spacing must fit in {width}"
            );
        }
    }

    #[test]
    fn the_minimum_card_width_survives_a_narrow_terminal() {
        // Half of 40 is 20, below the 30-cell minimum: the minimum wins.
        assert_eq!(grid_card_width(40, 1), CARD_MIN_WIDTH);
        // Narrower than the minimum itself: the card takes what exists.
        assert_eq!(grid_card_width(20, 1), 20);
    }

    #[test]
    fn descriptions_are_truncated_with_an_ellipsis_to_the_card_width() {
        assert_eq!(truncate_to_width("short", 28), "short");
        assert_eq!(
            truncate_to_width("a very long description that cannot fit", 12),
            "a very long\u{2026}"
        );
        // Width-aware: wide characters count as two cells.
        assert_eq!(truncate_to_width("日本語テスト", 5), "日本\u{2026}");
    }

    // ─── WI 0116 §6b: the card's `Env` row and the height it costs ──────────

    fn task_with_unmet(unmet: &[&str]) -> Task {
        Task {
            unmet_env: unmet.iter().map(|s| s.to_string()).collect(),
            ..task_with(TaskStatus::Active, None, false, false)
        }
    }

    /// The extra row is measured across the **whole grid**, because cards share
    /// layout rows: one affected task grows every card by one, so the grid does
    /// not reflow every time the daemon's coverage changes.
    #[test]
    fn the_grid_grows_one_row_when_any_task_has_an_unmet_name() {
        use super::{grid_card_height, CARD_MIN_HEIGHT};
        let clean = [task_with_unmet(&[]), task_with_unmet(&[])];
        assert_eq!(
            grid_card_height(&clean),
            CARD_MIN_HEIGHT,
            "with nothing unmet anywhere the grid lays out exactly as it did \
             before WI 0116"
        );
        let mixed = [task_with_unmet(&[]), task_with_unmet(&["AWS_PROFILE"])];
        assert_eq!(
            grid_card_height(&mixed),
            CARD_MIN_HEIGHT + 1,
            "one affected task grows every card, so the cards stay uniform"
        );
        assert_eq!(grid_card_height(&[]), CARD_MIN_HEIGHT);
    }

    /// **The `Env` row is not a `CardStatus` variant and must not become one.**
    ///
    /// That precedence table answers "what is this task's run state"; an unmet
    /// variable is orthogonal — a *paused* task can have one too — so
    /// overloading the card colour would make two unrelated facts compete for
    /// one channel. Every row of the table answers identically with and
    /// without an unmet name.
    #[test]
    fn an_unmet_env_name_never_changes_the_card_status() {
        use TaskStatus::{Active, Paused};
        for (base, expected) in [
            (task_with(Paused, None, false, false), CardStatus::Paused),
            (
                task_with(Active, Some(RunStatus::Running), true, false),
                CardStatus::Running,
            ),
            (
                task_with(Active, Some(RunStatus::Failed), true, false),
                CardStatus::Failed,
            ),
            (task_with(Active, None, false, false), CardStatus::NeverRun),
            (
                task_with(Active, Some(RunStatus::WorkflowExecuted), true, false),
                CardStatus::Active,
            ),
        ] {
            let unmet = Task {
                unmet_env: vec!["AWS_PROFILE".to_string()],
                ..base.clone()
            };
            assert_eq!(card_status(&base), expected);
            assert_eq!(
                card_status(&unmet),
                expected,
                "an unmet variable must not move the card's colour"
            );
        }
    }
}
