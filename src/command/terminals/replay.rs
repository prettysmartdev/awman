//! The viewer side of terminal history replay (WI 0105, Layer 2).
//!
//! A viewer that subscribes with `history=true` receives, in order:
//! `History*` (retained raw output) → `Snapshot` (the current screen) →
//! live `Output…`. [`ReplayFeed`] turns those frames into the bytes a viewer
//! writes, through **one** output filter instance for history, snapshot and
//! live output alike, so replayed bytes are filtered exactly as live bytes.
//!
//! Rules:
//! - History is real output: the viewer may scroll it into scrollback.
//! - When the first history frame is `truncated`, one dim marker line is
//!   written before it (after the filter; it is not agent output).
//! - Before the snapshot that follows history, `ESC[0m ESC[r` resets
//!   attributes and the scroll region. The snapshot is filtered by an
//!   isolated filter instance ([`ReplayFilter::feed_snapshot`]), so a
//!   sequence or UTF-8 character the producer left open at the subscribe
//!   point neither swallows the repaint nor is reset: the live continuation
//!   is read as one uninterrupted stream (an incomplete trailing character
//!   is held back and re-emitted after the repaint). The hub's snapshot
//!   restores the program's scroll margins. A snapshot captured at another
//!   size than the viewer's is re-rendered for it as a screen replacement
//!   (`fit_snapshot`), so it never scrolls into the viewer's scrollback.
//! - A truncated history is also reported once ([`ReplayEffect::Truncated`]),
//!   since the repaint can erase the marker line.
//! - `Lagged` is reported once per feed so the viewer can say output was
//!   skipped; lag recovery carries no history.

use crate::data::terminal::TerminalFrame;

/// The line written before a truncated history (dim, then reset). Plain
/// ASCII: Layer 2 draws no box characters.
pub const EARLIER_OUTPUT_MARKER: &[u8] = b"\x1b[2m-- earlier output not retained --\x1b[0m\r\n";

/// Written between history and the snapshot that follows it.
pub const HISTORY_SNAPSHOT_RESET: &[u8] = b"\x1b[0m\x1b[r";

/// What a viewer status line says after the first `Lagged`.
pub const LAGGED_MESSAGE: &str = "viewer fell behind; some output was skipped";

/// What a viewer status line says when its history starts after evicted (or
/// withheld) output. The in-stream marker line can be repainted away by the
/// snapshot that follows, so viewers also report this once as a message.
pub const TRUNCATED_MESSAGE: &str = "earlier output was not retained; scrollback starts later";

/// The output filter a viewer already applies to live bytes.
pub trait ReplayFilter {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>);
    /// Drop (never flush raw) a sequence held across chunks.
    fn finish(&mut self, out: &mut Vec<u8>);
    /// Filter a snapshot repaint **without** touching the live stream's
    /// state: a snapshot is inserted at an arbitrary producer byte boundary
    /// (mid UTF-8 character or mid escape sequence), and the live
    /// continuation must still be interpreted as one uninterrupted stream.
    /// Filters with state use a second instance here.
    fn feed_snapshot(&mut self, screen: &[u8], out: &mut Vec<u8>) {
        self.feed(screen, out);
    }
}

/// No filtering: for viewers whose renderer parses bytes itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unfiltered;

impl ReplayFilter for Unfiltered {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(chunk);
    }
    fn finish(&mut self, _out: &mut Vec<u8>) {}
}

/// What one frame meant for the viewer, beyond the bytes it wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayEffect {
    /// Nothing besides any bytes written.
    None,
    /// The first `Lagged` of this feed: tell the user output was skipped.
    FirstLag,
    /// The history starts after output that was not retained: tell the user
    /// (once per feed) in a place the snapshot repaint cannot erase.
    Truncated,
}

/// One viewer stream's replay state. Create one per connection whose
/// emulator is fresh; keep the same filter for the connection's lifetime.
#[derive(Debug, Default)]
pub struct ReplayFeed {
    history_frames: u64,
    /// History was written and its snapshot has not arrived yet.
    in_history: bool,
    lag_reported: bool,
    /// An incomplete UTF-8 character at the end of history, held back so the
    /// snapshot repaint cannot interrupt it; re-emitted after the repaint,
    /// where the live continuation completes it.
    pending_utf8: Vec<u8>,
    /// The viewer's screen (cols, rows) when known: a snapshot captured at
    /// another size is re-rendered for it as a screen replacement.
    viewer_size: Option<(u16, u16)>,
}

impl ReplayFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the viewer's current screen size (initially and on resize).
    pub fn set_viewer_size(&mut self, cols: u16, rows: u16) {
        self.viewer_size = Some((cols, rows));
    }

    /// A snapshot captured at `from` (cols, rows), fitted to the viewer.
    pub fn sized_snapshot(
        &mut self,
        screen: &[u8],
        from: (u16, u16),
        filter: &mut dyn ReplayFilter,
        out: &mut Vec<u8>,
    ) {
        match self.viewer_size {
            Some(to) if to != from => {
                let fitted = crate::engine::terminal_hub::fit_snapshot(screen, from, to);
                self.snapshot(&fitted, filter, out);
            }
            _ => self.snapshot(screen, filter, out),
        }
    }

    /// Whether any history frame was received on this feed.
    pub fn received_history(&self) -> bool {
        self.history_frames > 0
    }

    pub fn history(
        &mut self,
        bytes: &[u8],
        truncated: bool,
        filter: &mut dyn ReplayFilter,
        out: &mut Vec<u8>,
    ) -> bool {
        let marked = self.history_frames == 0 && truncated;
        if marked {
            out.extend_from_slice(EARLIER_OUTPUT_MARKER);
        }
        self.history_frames += 1;
        self.in_history = true;
        out.append(&mut self.pending_utf8);
        filter.feed(bytes, out);
        let keep = incomplete_utf8_tail(out);
        self.pending_utf8 = out.split_off(out.len() - keep);
        marked
    }

    pub fn snapshot(&mut self, screen: &[u8], filter: &mut dyn ReplayFilter, out: &mut Vec<u8>) {
        if std::mem::take(&mut self.in_history) {
            out.extend_from_slice(HISTORY_SNAPSHOT_RESET);
        }
        // Never `finish` the live filter here: that would reset its state at
        // an arbitrary byte boundary and misread the live continuation (a
        // UTF-8 continuation byte taken for an APC introducer swallows all
        // later output). The repaint goes through an isolated filter.
        filter.feed_snapshot(screen, out);
        out.append(&mut self.pending_utf8);
    }

    pub fn output(&mut self, bytes: &[u8], filter: &mut dyn ReplayFilter, out: &mut Vec<u8>) {
        self.in_history = false;
        out.append(&mut self.pending_utf8);
        filter.feed(bytes, out);
    }

    /// `true` the first time only.
    pub fn lagged(&mut self) -> bool {
        !std::mem::replace(&mut self.lag_reported, true)
    }

    /// Apply one frame: history, snapshot and output write bytes to `out`;
    /// every other frame writes nothing (the caller handles it).
    pub fn apply(
        &mut self,
        frame: &TerminalFrame,
        filter: &mut dyn ReplayFilter,
        out: &mut Vec<u8>,
    ) -> ReplayEffect {
        match frame {
            TerminalFrame::History { bytes, truncated } => {
                if self.history(bytes, *truncated, filter, out) {
                    return ReplayEffect::Truncated;
                }
            }
            TerminalFrame::Snapshot {
                screen, cols, rows, ..
            } => self.sized_snapshot(screen, (*cols, *rows), filter, out),
            TerminalFrame::Output { bytes } => self.output(bytes, filter, out),
            TerminalFrame::Lagged if self.lagged() => return ReplayEffect::FirstLag,
            _ => {}
        }
        ReplayEffect::None
    }
}

/// Length of an incomplete UTF-8 character at the end of `bytes` (a lead
/// byte followed by fewer continuation bytes than it announces), else 0.
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        return if back < needed { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod replay_feed_tests {
    use super::*;

    /// Drops everything between `ESC ]` and BEL, holding a partial one
    /// across chunks, like the host filter does for OSC 52.
    #[derive(Default)]
    struct DropOsc {
        held: Vec<u8>,
        inside: bool,
    }
    impl ReplayFilter for DropOsc {
        fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
            for &b in chunk {
                if self.inside {
                    if b == 0x07 {
                        self.inside = false;
                        self.held.clear();
                    } else {
                        self.held.push(b);
                    }
                } else if self.held == [0x1b] {
                    self.held.clear();
                    if b == b']' {
                        self.inside = true;
                    } else {
                        out.extend_from_slice(&[0x1b, b]);
                    }
                } else if b == 0x1b {
                    self.held.push(b);
                } else {
                    out.push(b);
                }
            }
        }
        fn feed_snapshot(&mut self, screen: &[u8], out: &mut Vec<u8>) {
            // As the production filters do: a second instance for repaints.
            DropOsc::default().feed(screen, out);
        }
        fn finish(&mut self, _out: &mut Vec<u8>) {
            self.held.clear();
            self.inside = false;
        }
    }

    fn snapshot(screen: &[u8]) -> TerminalFrame {
        TerminalFrame::Snapshot {
            has_output: true,
            cols: 80,
            rows: 24,
            screen: screen.to_vec(),
        }
    }

    fn history(bytes: &[u8], truncated: bool) -> TerminalFrame {
        TerminalFrame::History {
            bytes: bytes.to_vec(),
            truncated,
        }
    }

    #[test]
    fn history_then_reset_then_snapshot_then_output() {
        let mut feed = ReplayFeed::new();
        let mut filter = Unfiltered;
        let mut out = Vec::new();
        for frame in [
            history(b"old 1\r\n", false),
            history(b"old 2\r\n", false),
            snapshot(b"<screen>"),
            TerminalFrame::Output {
                bytes: b"live".to_vec(),
            },
        ] {
            assert_eq!(
                feed.apply(&frame, &mut filter, &mut out),
                ReplayEffect::None
            );
        }
        assert!(feed.received_history());
        assert_eq!(out, b"old 1\r\nold 2\r\n\x1b[0m\x1b[r<screen>live");
    }

    #[test]
    fn truncated_history_gets_one_marker_first() {
        let mut feed = ReplayFeed::new();
        let mut out = Vec::new();
        feed.apply(&history(b"a\r\n", true), &mut Unfiltered, &mut out);
        feed.apply(&history(b"b\r\n", false), &mut Unfiltered, &mut out);
        let mut expected = EARLIER_OUTPUT_MARKER.to_vec();
        expected.extend_from_slice(b"a\r\nb\r\n");
        assert_eq!(out, expected);
        // A withheld history is the marker only.
        let mut feed = ReplayFeed::new();
        let mut out = Vec::new();
        feed.apply(&history(b"", true), &mut Unfiltered, &mut out);
        assert_eq!(out, EARLIER_OUTPUT_MARKER);
    }

    #[test]
    fn a_snapshot_without_history_is_unchanged() {
        let mut feed = ReplayFeed::new();
        let mut out = Vec::new();
        feed.apply(&snapshot(b"<screen>"), &mut Unfiltered, &mut out);
        assert_eq!(out, b"<screen>");
        // A later lag snapshot never gets the history reset either.
        assert_eq!(
            feed.apply(&TerminalFrame::Lagged, &mut Unfiltered, &mut out),
            ReplayEffect::FirstLag
        );
        assert_eq!(
            feed.apply(&TerminalFrame::Lagged, &mut Unfiltered, &mut out),
            ReplayEffect::None
        );
        feed.apply(&snapshot(b"<again>"), &mut Unfiltered, &mut out);
        assert_eq!(out, b"<screen><again>");
    }

    #[test]
    fn replayed_bytes_go_through_the_same_filter_even_split_across_frames() {
        let mut feed = ReplayFeed::new();
        let mut filter = DropOsc::default();
        let mut out = Vec::new();
        feed.apply(
            &history(b"keep\x1b]52;c;c2Vj", false),
            &mut filter,
            &mut out,
        );
        feed.apply(&history(b"cmV0\x07tail\r\n", false), &mut filter, &mut out);
        feed.apply(&snapshot(b"S"), &mut filter, &mut out);
        feed.apply(
            &TerminalFrame::Output {
                bytes: b"\x1b]52;c;eA==\x07live".to_vec(),
            },
            &mut filter,
            &mut out,
        );
        assert_eq!(out, b"keeptail\r\n\x1b[0m\x1b[rSlive");
    }

    #[test]
    fn an_open_sequence_at_the_end_of_history_cannot_swallow_the_repaint() {
        let mut feed = ReplayFeed::new();
        let mut filter = DropOsc::default();
        let mut out = Vec::new();
        feed.apply(
            &history(b"text\x1b]52;c;unterminated", false),
            &mut filter,
            &mut out,
        );
        feed.apply(&snapshot(b"SCREEN"), &mut filter, &mut out);
        assert_eq!(out, b"text\x1b[0m\x1b[rSCREEN");
        // The live stream is still inside that OSC: its continuation stays
        // dropped, exactly as for a viewer attached all along, and text
        // after the terminator is delivered.
        out.clear();
        feed.apply(
            &TerminalFrame::Output {
                bytes: b"-rest\x07after".to_vec(),
            },
            &mut filter,
            &mut out,
        );
        assert_eq!(out, b"after");
    }

    #[test]
    fn a_utf8_character_split_at_the_snapshot_is_completed_after_the_repaint() {
        let mut feed = ReplayFeed::new();
        let mut out = Vec::new();
        feed.apply(&history(b"hello \xf0", false), &mut Unfiltered, &mut out);
        feed.apply(&snapshot(b"S"), &mut Unfiltered, &mut out);
        feed.apply(
            &TerminalFrame::Output {
                bytes: b"\x9f\x98\x80done".to_vec(),
            },
            &mut Unfiltered,
            &mut out,
        );
        assert_eq!(out, b"hello \x1b[0m\x1b[rS\xf0\x9f\x98\x80done");
    }
}
