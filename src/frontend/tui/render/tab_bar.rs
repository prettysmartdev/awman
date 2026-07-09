//! Tab-bar widget rendering.

use super::*;

/// Render the tab bar — matches old amux:
/// - 3-row tall cells with rounded borders
/// - Active tab: omits the bottom border so it visually merges into the
///   execution window below; title gets `➡` prefix and is bold + tab color
/// - Inactive tab: full borders; title is DarkGray (subdued)
/// - Subcommand label rendered INSIDE the cell as content (1 row),
///   not in the title
/// - Width is derived from each tab's natural content width, capped against
///   the budget (¼/½/¾/1/n for n=1/2/3/n tabs)
pub(super) fn render_tab_bar(app: &App, area: Rect, frame: &mut Frame) {
    let n = app.tabs.len();
    if n == 0 || area.width == 0 {
        return;
    }

    // First pass: compute the maximum natural content width across all tabs.
    // We pass `u16::MAX` as the cell width to `tab_subcommand_label` so it
    // doesn't truncate while measuring.
    let max_natural_content: u16 = app
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let is_active = i == app.active_tab;
            // Measure with the untruncated project name so a wide terminal
            // lets long names claim more width; `compute_tab_bar_width`
            // still caps the result against the per-tab budget.
            let project = tab.project_name(u16::MAX);
            // Title interior: `" ➡ {project} "` = project + 4 chars
            // (or `" {project} "` = project + 2 chars when not active);
            // we always size for the wider variant so the active toggle
            // doesn't reflow the bar.
            let title_inner = (project.chars().count() as u16).saturating_add(4);
            let subcmd = tab.tab_subcommand_label(u16::MAX, is_active);
            // Body interior: `" {subcmd} "` = subcmd + 2 chars
            let content_inner = (subcmd.chars().count() as u16).saturating_add(2);
            title_inner.max(content_inner)
        })
        .max()
        .unwrap_or(18);

    let tab_width = compute_tab_bar_width(n, area.width, max_natural_content);
    if tab_width == 0 {
        return;
    }

    for (i, tab) in app.tabs.iter().enumerate() {
        let x = area.x + (i as u16) * tab_width;
        // Stop drawing when the next cell would overflow — old awman did the
        // same; there is no overflow indicator.
        if x + tab_width > area.x + area.width {
            break;
        }
        let is_active = i == app.active_tab;
        let tab_area = Rect::new(x, area.y, tab_width, 3);
        let color = tab_color(tab);
        let project = tab.project_name(tab_width);
        let subcmd = tab.tab_subcommand_label(tab_width, is_active);

        let (border_style, title_style, content_style) = if is_active {
            (
                Style::default().fg(color),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        } else {
            (
                Style::default().fg(color),
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::DarkGray),
            )
        };

        let title_text = if is_active {
            format!(" \u{27a1} {} ", project)
        } else {
            format!(" {} ", project)
        };

        let borders = if is_active {
            Borders::TOP | Borders::LEFT | Borders::RIGHT
        } else {
            Borders::ALL
        };

        let block = Block::default()
            .title(Span::styled(title_text, title_style))
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(border_style);

        let content = Paragraph::new(Line::from(Span::styled(
            format!(" {} ", subcmd),
            content_style,
        )))
        .block(block);

        frame.render_widget(content, tab_area);
    }
}
