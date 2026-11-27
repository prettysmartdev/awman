//! WI 0105: a viewer that attaches late and replays history through the TUI
//! slot pipeline (`drain_slot_bytes`: alternate-screen strip, then
//! `RegionScrollEmulator` into a `vt100` parser with scrollback) ends with the
//! same scrollback and visible screen as a viewer attached all along. The
//! snapshot repaint after history adds no scrollback rows.

use std::sync::Arc;

use super::AltScreenStripper;
use crate::command::instance::frontend::{HostByteFilter, InstancePresentation};
use crate::command::terminals::{ReplayFeed, ReplayFilter, EARLIER_OUTPUT_MARKER};
use crate::data::daemon_state::FeatureName;
use crate::data::ids::TabId;
use crate::data::terminal::{HostOutputPolicy, TerminalFrame, TerminalOwner, TerminalOwnerInfo};
use crate::engine::terminal_hub::{
    input_channel, SubscribeOptions, TerminalHandle, TerminalHub, TerminalHubConfig, TerminalInput,
    TerminalRegistration,
};
use crate::frontend::tui::command_frontend::TuiPresentation;
use crate::frontend::tui::region_scroll::RegionScrollEmulator;

const COLS: u16 = 40;
const ROWS: u16 = 6;
const SCROLLBACK: usize = 10_000;

/// One TUI container slot's renderer, exactly as `drain_slot_bytes` feeds it.
struct Slot {
    parser: vt100::Parser,
    region: RegionScrollEmulator,
    alt: AltScreenStripper,
}

impl Slot {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new(ROWS, COLS, SCROLLBACK),
            region: RegionScrollEmulator::new(),
            alt: AltScreenStripper::default(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let filtered = self.alt.strip(bytes);
        self.region.process(&mut self.parser, &filtered.bytes);
    }

    /// Every line, oldest scrollback row first, then the visible rows.
    fn all_rows(&mut self) -> Vec<String> {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let depth = screen.scrollback();
        let mut rows = Vec::new();
        for offset in (1..=depth).rev() {
            screen.set_scrollback(offset);
            rows.push(screen.rows(0, COLS).next().unwrap_or_default());
        }
        screen.set_scrollback(0);
        rows.extend(screen.rows(0, COLS));
        rows
    }

    fn scrollback_depth(&mut self) -> usize {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let depth = screen.scrollback();
        screen.set_scrollback(0);
        depth
    }

    fn visible(&self) -> String {
        self.parser.screen().contents()
    }
}

/// The TUI bridge's filter pair: one for the producer stream, one for
/// snapshot repaints (as `per_command::attach::SlotFilter`).
struct Filter(Box<dyn HostByteFilter>, Box<dyn HostByteFilter>);
impl Filter {
    fn tui() -> Self {
        Self(
            TuiPresentation.host_output_filter(HostOutputPolicy::default()),
            TuiPresentation.host_output_filter(HostOutputPolicy::default()),
        )
    }
}
impl ReplayFilter for Filter {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.0.feed(chunk, out);
    }
    fn finish(&mut self, out: &mut Vec<u8>) {
        self.0.finish(out);
    }
    fn feed_snapshot(&mut self, screen: &[u8], out: &mut Vec<u8>) {
        self.1.feed(screen, out);
        self.1.finish(out);
    }
}

fn terminal(ring_bytes: usize) -> (Arc<TerminalHub>, TerminalHandle) {
    terminal_sized(ring_bytes, COLS, ROWS)
}

fn terminal_sized(ring_bytes: usize, cols: u16, rows: u16) -> (Arc<TerminalHub>, TerminalHandle) {
    let hub = Arc::new(TerminalHub::new(TerminalHubConfig {
        ring_bytes,
        subscriber_capacity: 4096,
        ..Default::default()
    }));
    let (stdin, input) = input_channel();
    std::mem::forget(input);
    let handle = hub.register(
        TerminalRegistration {
            automation: None,
            owner: TerminalOwnerInfo {
                owner: TerminalOwner::Tab(TabId::new()),
                feature: FeatureName::Engine,
                source: None,
                attempt: 1,
                allow_remote_input: false,
            },
            agent: "fixture".into(),
            model: None,
            cols,
            rows,
        },
        TerminalInput {
            stdin,
            resize: None,
        },
    );
    (hub, handle)
}

/// Attach late with history and drain every queued frame into a slot.
fn attach_late(hub: &TerminalHub, handle: &TerminalHandle) -> (Slot, Vec<TerminalFrame>) {
    let mut sub = hub
        .subscribe_with(handle.id(), SubscribeOptions { history: true })
        .unwrap();
    let mut frames = Vec::new();
    // Everything up to the snapshot is queued synchronously.
    loop {
        let frame = futures_util::FutureExt::now_or_never(sub.next())
            .flatten()
            .expect("queued frame");
        let done = matches!(frame, TerminalFrame::Snapshot { .. });
        frames.push(frame);
        if done {
            break;
        }
    }
    let mut slot = Slot::new();
    let mut feed = ReplayFeed::new();
    let mut filter = Filter::tui();
    for frame in &frames {
        let mut out = Vec::new();
        feed.apply(frame, &mut filter, &mut out);
        slot.feed(&out);
    }
    (slot, frames)
}

fn mirror_screen(hub: &TerminalHub, handle: &TerminalHandle) -> String {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(&hub.snapshot(handle.id()).unwrap().screen);
    parser.screen().contents()
}

#[test]
fn late_viewer_scrollback_matches_a_viewer_attached_all_along() {
    let (hub, handle) = terminal(256 * 1024);
    let mut always = Slot::new();
    let output: Vec<Vec<u8>> = (0..30)
        .map(|n| format!("\x1b[1mline\x1b[0m {n:02}\r\n").into_bytes())
        .collect();
    for chunk in &output {
        handle.push_output(chunk);
        always.feed(chunk);
    }
    let (mut late, _) = attach_late(&hub, &handle);
    assert_eq!(late.all_rows(), always.all_rows());
    assert_eq!(late.scrollback_depth(), always.scrollback_depth());
    assert_eq!(late.visible(), mirror_screen(&hub, &handle));
    let rows = late.all_rows();
    assert_eq!(rows[0], "line 00");
    assert!(rows.iter().any(|row| row == "line 29"));
}

#[test]
fn the_snapshot_repaint_adds_no_scrollback_rows() {
    let (hub, handle) = terminal(256 * 1024);
    for n in 0..10 {
        handle.push_output(format!("row {n}\r\n").as_bytes());
    }
    let (mut late, frames) = attach_late(&hub, &handle);
    // History alone, without the snapshot:
    let mut history_only = Slot::new();
    for frame in &frames {
        if let TerminalFrame::History { bytes, .. } = frame {
            history_only.feed(bytes);
        }
    }
    assert_eq!(late.scrollback_depth(), history_only.scrollback_depth());
    assert_eq!(late.visible(), history_only.visible());
}

#[test]
fn region_scroll_history_then_snapshot_is_consistent() {
    // A codex-style program: a top-anchored scroll region (rows 1-4) above a
    // fixed footer, lines scrolled inside the region.
    let (hub, handle) = terminal(256 * 1024);
    let mut always = Slot::new();
    let mut chunks = vec![b"\x1b[2J\x1b[6;1Hfooter\x1b[1;4r\x1b[1;1H".to_vec()];
    for n in 0..12 {
        chunks.push(format!("msg {n:02}\r\n").into_bytes());
    }
    for chunk in &chunks {
        handle.push_output(chunk);
        always.feed(chunk);
    }
    let (mut late, _) = attach_late(&hub, &handle);
    assert_eq!(late.scrollback_depth(), always.scrollback_depth());
    assert_eq!(late.all_rows(), always.all_rows());
    assert_eq!(late.visible(), mirror_screen(&hub, &handle));
    // Live output continues inside the program's region (it re-arms it,
    // as a redraw-driven program does) and both viewers agree again.
    let live = b"\x1b[1;4r\x1b[4;1Hnext\r\n".to_vec();
    handle.push_output(&live);
    late.feed(&live);
    always.feed(&live);
    assert_eq!(late.visible(), always.visible());
    assert_eq!(late.scrollback_depth(), always.scrollback_depth());
}

#[test]
fn evicted_history_starts_with_the_marker_and_on_a_line_boundary() {
    let (hub, handle) = terminal(256);
    for n in 0..100 {
        handle.push_output(format!("line {n:03}\r\n").as_bytes());
    }
    let (mut late, frames) = attach_late(&hub, &handle);
    assert!(matches!(
        frames[0],
        TerminalFrame::History {
            truncated: true,
            ..
        }
    ));
    let rows = late.all_rows();
    let marker = String::from_utf8_lossy(EARLIER_OUTPUT_MARKER);
    assert!(marker.contains(&rows[0]), "{rows:?}");
    assert!(rows[1].starts_with("line "), "{rows:?}");
    assert!(rows.iter().any(|row| row == "line 099"));
    assert_eq!(late.visible(), mirror_screen(&hub, &handle));
}

#[test]
fn alternate_screen_history_accumulates_like_a_continuous_viewer() {
    // Not evicted: the full-screen program's frames reach the slot exactly as
    // for a viewer attached all along (alt-screen switches are stripped).
    let (hub, handle) = terminal(256 * 1024);
    let mut always = Slot::new();
    let mut chunks = vec![b"shell$ top\r\n\x1b[?1049h".to_vec()];
    for n in 0..3 {
        chunks.push(format!("\x1b[H\x1b[2Jframe {n}\r\n").into_bytes());
    }
    for chunk in &chunks {
        handle.push_output(chunk);
        always.feed(chunk);
    }
    let (mut late, _) = attach_late(&hub, &handle);
    assert_eq!(late.all_rows(), always.all_rows());
    assert_eq!(late.visible(), mirror_screen(&hub, &handle));
}

#[test]
fn host_filtered_sequences_in_history_are_stripped_like_live_output() {
    let (hub, handle) = terminal(256 * 1024);
    // An OSC 52 clipboard write split across two pushes (and so possibly two
    // history frames), and a desktop notification.
    handle.push_output(b"before\x1b]52;c;aGVs");
    handle.push_output(b"bG8=\x07after\r\n\x1b]9;ping\x07done\r\n");
    let mut sub = hub
        .subscribe_with(handle.id(), SubscribeOptions { history: true })
        .unwrap();
    let mut feed = ReplayFeed::new();
    let mut filter = Filter::tui();
    let mut history = Vec::new();
    while let Some(frame) = futures_util::FutureExt::now_or_never(sub.next()).flatten() {
        if matches!(frame, TerminalFrame::Snapshot { .. }) {
            break;
        }
        feed.apply(&frame, &mut filter, &mut history);
    }
    let text = String::from_utf8_lossy(&history);
    assert!(text.contains("beforeafter\r\n"), "{text:?}");
    assert!(text.contains("done\r\n"));
    assert!(!text.contains("]52"), "{text:?}");
    assert!(!text.contains("]9;"), "{text:?}");
}

/// A late viewer that keeps its subscription, feed and filter, so live output
/// after attach continues through the same production pipeline.
struct LateViewer {
    sub: crate::engine::terminal_hub::TerminalSubscription,
    feed: ReplayFeed,
    filter: Filter,
    slot: Slot,
    effects: Vec<crate::command::terminals::ReplayEffect>,
}

impl LateViewer {
    fn attach(hub: &TerminalHub, handle: &TerminalHandle) -> Self {
        let sub = hub
            .subscribe_with(handle.id(), SubscribeOptions { history: true })
            .unwrap();
        let mut viewer = Self {
            sub,
            feed: ReplayFeed::new(),
            filter: Filter::tui(),
            slot: Slot::new(),
            effects: Vec::new(),
        };
        viewer.feed.set_viewer_size(COLS, ROWS);
        viewer.drain();
        viewer
    }

    /// Apply every frame queued so far.
    fn drain(&mut self) {
        while let Some(frame) = futures_util::FutureExt::now_or_never(self.sub.next()).flatten() {
            let mut out = Vec::new();
            let effect = self.feed.apply(&frame, &mut self.filter, &mut out);
            self.effects.push(effect);
            self.slot.feed(&out);
        }
    }
}

#[test]
fn alternate_screen_switches_split_at_every_byte_never_reach_the_parser() {
    for seq in [&b"\x1b[?1049h"[..], b"\x1b[?47h", b"\x1b[?1047h"] {
        for split in 1..seq.len() {
            let mut slot = Slot::new();
            let mut first = b"line\r\n".to_vec();
            first.extend_from_slice(&seq[..split]);
            let mut second = seq[split..].to_vec();
            second.extend_from_slice(b"inside\r\n");
            slot.feed(&first);
            slot.feed(&second);
            assert!(
                !slot.parser.screen().alternate_screen(),
                "split {split} of {seq:?} entered the alternate grid"
            );
            assert!(slot.visible().contains("inside"));
        }
    }
}

#[test]
fn a_switch_across_the_16k_history_frame_boundary_keeps_scrollback() {
    // 16 380 newlines fill the first history frame up to 4 bytes short of its
    // 16 KiB bound, so the switch is split across two frames.
    let (hub, handle) = terminal(256 * 1024);
    handle.push_output(&vec![b'\n'; 16_380]);
    let mut tail = b"\x1b[?1049h".to_vec();
    for n in 0..20 {
        tail.extend_from_slice(format!("frame {n}\r\n").as_bytes());
    }
    handle.push_output(&tail);
    let mut late = LateViewer::attach(&hub, &handle);
    assert!(!late.slot.parser.screen().alternate_screen());
    assert!(late.slot.scrollback_depth() > 0);
}

#[test]
fn attaching_mid_utf8_or_mid_osc_keeps_the_live_stream_intact() {
    let (hub, handle) = terminal(256 * 1024);
    handle.push_output(b"hello \xf0");
    let mut late = LateViewer::attach(&hub, &handle);
    handle.push_output(b"\x9f\x98\x80done");
    late.drain();
    assert!(
        late.slot.visible().contains("hello 😀done"),
        "{:?}",
        late.slot.visible()
    );

    // An OSC (title) left open at attach completes live; later plain text,
    // with no terminator of its own, is still delivered.
    let (hub, handle) = terminal(256 * 1024);
    handle.push_output(b"start \x1b]0;ti");
    let mut late = LateViewer::attach(&hub, &handle);
    handle.push_output(b"tle\x07after");
    late.drain();
    handle.push_output(b" more");
    late.drain();
    assert!(
        late.slot.visible().contains("after more"),
        "{:?}",
        late.slot.visible()
    );
}

#[test]
fn live_continuation_keeps_the_programs_scroll_region_after_replay() {
    let (hub, handle) = terminal(256 * 1024);
    let mut always = Slot::new();
    let mut chunks = vec![b"\x1b[2J\x1b[6;1Hfooter\x1b[1;4r\x1b[1;1H".to_vec()];
    for n in 0..4 {
        chunks.push(format!("msg{n:02}\r\n").into_bytes());
    }
    for chunk in &chunks {
        handle.push_output(chunk);
        always.feed(chunk);
    }
    let mut late = LateViewer::attach(&hub, &handle);
    // Plain output, without re-arming the region, several scrolls.
    for line in ["next\r\n", "again\r\n", "third\r\n"] {
        handle.push_output(line.as_bytes());
        always.feed(line.as_bytes());
    }
    late.drain();
    assert_eq!(late.slot.visible(), always.visible());
    assert!(late.slot.visible().contains("footer"));
}

#[test]
fn a_truncated_history_is_reported_even_when_the_snapshot_erases_its_marker() {
    // Withheld: alternate screen plus eviction.
    let (hub, handle) = terminal(32);
    handle.push_output(b"\x1b[?1049h");
    handle.push_output(&[b'#'; 64]);
    let late = LateViewer::attach(&hub, &handle);
    assert!(late
        .effects
        .contains(&crate::command::terminals::ReplayEffect::Truncated));
    // Evicted, short history.
    let (hub, handle) = terminal(64);
    for n in 0..20 {
        handle.push_output(format!("l{n}\r\n").as_bytes());
    }
    let late = LateViewer::attach(&hub, &handle);
    assert_eq!(
        late.effects
            .iter()
            .filter(|e| **e == crate::command::terminals::ReplayEffect::Truncated)
            .count(),
        1
    );
}

#[test]
fn a_snapshot_from_a_larger_terminal_adds_no_scrollback_in_a_smaller_slot() {
    // 35 lines of 79 printable columns on an 80x24 terminal, replayed into a
    // 40x6 slot: the snapshot must replace the screen, not scroll it.
    let (hub, handle) = terminal_sized(256 * 1024, 80, 24);
    for n in 0..35 {
        handle.push_output(format!("{n:02}{}\r\n", "y".repeat(77)).as_bytes());
    }
    let mut sub = hub
        .subscribe_with(handle.id(), SubscribeOptions { history: true })
        .unwrap();
    let mut feed = ReplayFeed::new();
    feed.set_viewer_size(COLS, ROWS);
    let mut filter = Filter::tui();
    let mut with_snapshot = Slot::new();
    let mut history_only = Slot::new();
    while let Some(frame) = futures_util::FutureExt::now_or_never(sub.next()).flatten() {
        let mut out = Vec::new();
        feed.apply(&frame, &mut filter, &mut out);
        with_snapshot.feed(&out);
        if matches!(frame, TerminalFrame::History { .. }) {
            history_only.feed(&out);
        }
        if matches!(frame, TerminalFrame::Snapshot { .. }) {
            break;
        }
    }
    assert_eq!(
        with_snapshot.scrollback_depth(),
        history_only.scrollback_depth()
    );
    // The fitted screen shows the newest rows, cropped to the slot width.
    assert!(
        with_snapshot.visible().contains("34yyyy"),
        "{:?}",
        with_snapshot.visible()
    );
}
