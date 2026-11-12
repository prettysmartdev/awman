//! `AcpFrontend` impl for the CLI.
//!
//! Renders ACP session updates as short, human-readable lines on stdout —
//! never raw JSON — and drives permission prompts / follow-up turns from
//! stdin. This is the line-oriented, cooked-mode counterpart to the TUI's
//! agent window: no raw mode, no PTY, just read/print like the rest of the
//! CLI's interactive Q&A (see `per_command/helpers.rs`).
//!
//! Headless runs (`-n`/`--headless`, and `--yolo`/`--auto` which the
//! engine driver already handles by never calling `request_permission` at
//! all) must never block on stdin: `non_interactive` degrades
//! `request_permission` to auto-approve and `next_prompt` to "no follow-up",
//! exactly mirroring how `take_non_interactive_io` degrades the stdio
//! `AgentFrontend` path in `container_frontend_marker.rs`.

use std::io::Write;

use crate::data::terminal::{HostOutputPolicy, ProgramStatusMode};
use crate::engine::acp::protocol::{sanitize_terminal_text, ContentChunk, ToolCallLocation};
use crate::engine::acp::{
    AcpFrontend, AvailableCommand, ContentBlock, PermissionDecision, PermissionOption,
    PermissionRequest, PlanEntry, SessionUpdate, ToolCall, ToolCallUpdate,
};
use crate::frontend::terminal_output_filter::{filter_all, HostOutputFilter};

use crate::frontend::cli::command_frontend::CliFrontend;

impl AcpFrontend for CliFrontend {
    fn render_update(&mut self, update: SessionUpdate) {
        let stream = match &update {
            SessionUpdate::AgentMessageChunk { chunk } => Some((chunk, false)),
            SessionUpdate::AgentThoughtChunk { chunk } => Some((chunk, true)),
            _ => None,
        };
        if let Some((
            ContentChunk {
                content: ContentBlock::Text { text },
                ..
            },
            dim,
        )) = stream
        {
            let filter = self
                .acp_output_filter
                .get_or_insert_with(|| HostOutputFilter::new(text_policy()));
            let safe = stream_text(filter, text);
            if dim {
                print!("\x1b[2m{safe}\x1b[0m");
            } else {
                print!("{safe}");
            }
            let _ = std::io::stdout().flush();
            return;
        }
        match render_update_text(&update) {
            RenderedUpdate::Stream(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            RenderedUpdate::Block(text) => {
                // Block text mixes awman's own labels (▸, brackets — all
                // printable) with agent-provided titles/paths/URIs; sanitising
                // the whole line strips any control chars the agent injected
                // without touching awman's formatting.
                println!("{}", sanitize_agent_text(&text));
            }
        }
    }

    fn request_permission(&mut self, request: PermissionRequest) -> PermissionDecision {
        if self.non_interactive {
            return self.headless.acp_permission(&request.options);
        }
        println!(
            "{}",
            sanitize_agent_text(&format_permission_request(&request))
        );
        loop {
            print!("choice: ");
            let _ = std::io::stdout().flush();
            match read_stdin_line() {
                None => return PermissionDecision::Cancelled,
                Some(input) => match match_permission_option(&request.options, &input) {
                    Some(option_id) => return PermissionDecision::Selected { option_id },
                    None => println!("awman: unrecognized choice {input:?}; try again"),
                },
            }
        }
    }

    fn next_prompt(&mut self) -> Option<String> {
        if self.non_interactive {
            return None;
        }
        print!("\n> ");
        let _ = std::io::stdout().flush();
        read_stdin_line()
    }
}

fn sanitize_agent_text(text: &str) -> String {
    let filtered = filter_all(text_policy(), text.as_bytes());
    sanitize_terminal_text(&String::from_utf8_lossy(&filtered))
}

fn text_policy() -> HostOutputPolicy {
    HostOutputPolicy {
        program_status: ProgramStatusMode::Strip,
        ..Default::default()
    }
}
fn stream_text(filter: &mut HostOutputFilter, text: &str) -> String {
    let mut safe = Vec::new();
    filter.feed(text.as_bytes(), &mut safe);
    sanitize_terminal_text(&String::from_utf8_lossy(&safe))
}

/// A single rendered update, split so the streamed-text case (message/thought
/// chunks, printed with no trailing newline as they arrive) is distinct from
/// the block case (tool calls, plans, available commands — each a complete,
/// newline-terminated line).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RenderedUpdate {
    Stream(String),
    Block(String),
}

pub(crate) fn render_update_text(update: &SessionUpdate) -> RenderedUpdate {
    match update {
        SessionUpdate::AgentMessageChunk { chunk } => render_content_chunk(chunk, false),
        SessionUpdate::AgentThoughtChunk { chunk } => render_content_chunk(chunk, true),
        SessionUpdate::ToolCall { tool_call } => RenderedUpdate::Block(format_tool_call(tool_call)),
        SessionUpdate::ToolCallUpdate { tool_call } => {
            RenderedUpdate::Block(format_tool_call_update(tool_call))
        }
        SessionUpdate::Plan { entries } => RenderedUpdate::Block(format_plan(entries)),
        SessionUpdate::AvailableCommandsUpdate { available_commands } => {
            RenderedUpdate::Block(format_available_commands(available_commands))
        }
    }
}

/// Message chunks stream raw text inline. Thought chunks stream too (ANSI
/// "dim" so they read as an aside, not the agent's actual reply) rather than
/// repeating a "[thinking]" label on every token. Non-text content within
/// either kind of chunk is rare and atomic, so it renders as its own block.
fn render_content_chunk(chunk: &ContentChunk, dim: bool) -> RenderedUpdate {
    match &chunk.content {
        ContentBlock::Text { text } => {
            // Strip agent-provided terminal-control characters (ANSI/OSC/etc.)
            // BEFORE wrapping with awman's own "dim" escape, so a malicious
            // agent can't smuggle escape sequences through the structured
            // renderer — but awman's intentional formatting is preserved.
            let text = sanitize_agent_text(text);
            let text = if dim {
                format!("\x1b[2m{text}\x1b[0m")
            } else {
                text
            };
            RenderedUpdate::Stream(text)
        }
        other => RenderedUpdate::Block(content_block_label(other)),
    }
}

fn content_block_label(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Image { mime_type, .. } => format!("[image: {mime_type}]"),
        ContentBlock::Audio { mime_type, .. } => format!("[audio: {mime_type}]"),
        ContentBlock::ResourceLink { name, uri, .. } => format!("[resource: {name} ({uri})]"),
        ContentBlock::Resource { .. } => "[resource]".to_string(),
    }
}

fn format_tool_call(tool_call: &ToolCall) -> String {
    tool_line(
        "tool",
        &tool_call.title,
        tool_call.kind.as_deref(),
        tool_call.status.as_deref(),
        &tool_call.locations,
    )
}

fn format_tool_call_update(update: &ToolCallUpdate) -> String {
    tool_line(
        "tool update",
        &tool_call_update_title(update),
        update.kind.as_deref(),
        update.status.as_deref(),
        update.locations.as_deref().unwrap_or(&[]),
    )
}

fn tool_call_update_title(update: &ToolCallUpdate) -> String {
    update
        .title
        .clone()
        .unwrap_or_else(|| format!("tool {}", update.tool_call_id))
}

fn tool_line(
    prefix: &str,
    title: &str,
    kind: Option<&str>,
    status: Option<&str>,
    locations: &[ToolCallLocation],
) -> String {
    let mut line = format!("▸ {prefix}: {title}");
    if let Some(kind) = kind {
        line.push_str(&format!(" [{kind}]"));
    }
    if let Some(status) = status {
        line.push_str(&format!(" — {status}"));
    }
    if !locations.is_empty() {
        let locs: Vec<String> = locations
            .iter()
            .map(|l| match l.line {
                Some(line_no) => format!("{}:{line_no}", l.path),
                None => l.path.clone(),
            })
            .collect();
        line.push_str(&format!(" ({})", locs.join(", ")));
    }
    line
}

fn format_plan(entries: &[PlanEntry]) -> String {
    let mut out = String::from("plan:");
    for entry in entries {
        out.push_str(&format!(
            "\n  [{}] {} ({})",
            entry.status, entry.content, entry.priority
        ));
    }
    out
}

fn format_available_commands(commands: &[AvailableCommand]) -> String {
    if commands.is_empty() {
        return "available commands: (none)".to_string();
    }
    let mut out = String::from("available commands:");
    for command in commands {
        out.push_str(&format!("\n  /{} — {}", command.name, command.description));
    }
    out
}

fn format_permission_request(request: &PermissionRequest) -> String {
    let mut out = format!(
        "permission requested: {}",
        tool_line(
            "tool",
            &tool_call_update_title(&request.tool_call),
            request.tool_call.kind.as_deref(),
            request.tool_call.status.as_deref(),
            request.tool_call.locations.as_deref().unwrap_or(&[]),
        )
    );
    for (i, option) in request.options.iter().enumerate() {
        out.push_str(&format!(
            "\n  [{}] {} ({})",
            i + 1,
            option.name,
            option.kind
        ));
    }
    out
}

fn match_permission_option(options: &[PermissionOption], input: &str) -> Option<String> {
    let trimmed = input.trim();
    if let Ok(index) = trimmed.parse::<usize>() {
        if index >= 1 {
            if let Some(option) = options.get(index - 1) {
                return Some(option.option_id.clone());
            }
        }
    }
    options
        .iter()
        .find(|o| o.option_id.eq_ignore_ascii_case(trimmed) || o.name.eq_ignore_ascii_case(trimmed))
        .map(|o| o.option_id.clone())
}

/// Reads one line from stdin, distinguishing EOF (Ctrl+D) from an empty
/// line: `None` on EOF or a read error, `Some(trimmed)` otherwise — even
/// `Some(String::new())` for a blank line. `per_command::helpers::read_line`
/// cannot be reused here because it collapses "not a TTY" and "EOF" into the
/// same `None`, while an ACP session must react to those differently (a
/// live-but-headless run degrades before ever calling this; a live
/// interactive run's EOF must end the session).
fn read_stdin_line() -> Option<String> {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) => None,
        Ok(_) => Some(buf.trim().to_string()),
        Err(_) => None,
    }
}
