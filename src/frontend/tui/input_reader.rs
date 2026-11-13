//! One input owner for the TUI. On Unix, filter probe replies from raw
//! terminal bytes before decoding keys; Crossterm's public reader hides DA1
//! and turns OSC replies into keystrokes, losing their ordering.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};

use super::program_status_emitter::ProgramStatusEmitter;

mod decode;

pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const ESCAPE_DELAY: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, PartialEq, Eq)]
enum DecodedEvent {
    Event(Event),
    CursorPosition(u16, u16),
    KeyboardEnhancementFlags(event::KeyboardEnhancementFlags),
    PrimaryDeviceAttributes,
}

/// Byte decoder separated from I/O so fake terminals exercise exactly the
/// filtering and decoding path used before the event loop's key handler.
#[derive(Default)]
struct InputDecoder {
    pending: Vec<u8>,
    events: VecDeque<Event>,
}

impl InputDecoder {
    fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.pending.push(*byte);
            self.parse(true);
            // Bound an incomplete CSI or paste without retaining unbounded
            // terminal input. No protocol reply is buffered here.
            if self.pending.len() > 1024 * 1024 {
                self.pending.clear();
            }
        }
    }

    fn parse(&mut self, more: bool) {
        match decode::parse_event(&self.pending, more) {
            Ok(Some(decoded)) => {
                match decoded {
                    DecodedEvent::Event(event) => self.events.push_back(event),
                    DecodedEvent::CursorPosition(x, y) => {
                        let _ = (x, y);
                    }
                    DecodedEvent::KeyboardEnhancementFlags(flags) => {
                        let _ = flags;
                    }
                    DecodedEvent::PrimaryDeviceAttributes => {}
                }
                self.pending.clear();
            }
            Err(_) => self.pending.clear(),
            Ok(None) => {}
        }
    }
}

pub(super) struct InputReader {
    #[cfg(unix)]
    tty: Option<std::fs::File>,
    decoder: InputDecoder,
    probe_enabled: bool,
    started: Instant,
    last_input: Instant,
    expired: bool,
    #[cfg(unix)]
    size: Option<(u16, u16)>,
}

impl InputReader {
    pub(super) fn new(probe_enabled: bool) -> io::Result<Self> {
        #[cfg(unix)]
        let tty = if probe_enabled {
            use std::io::IsTerminal;
            use std::os::unix::fs::OpenOptionsExt;
            let input = io::stdin();
            if input.is_terminal() {
                // Match Crossterm's input terminal. Poll before reading;
                // don't change O_NONBLOCK on a dup of the user's stdin.
                nix::unistd::dup(&input).map(std::fs::File::from).ok()
            } else {
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                    .open("/dev/tty")
                    .ok()
            }
        } else {
            None
        };
        #[cfg(unix)]
        let probe_enabled = probe_enabled && tty.is_some();
        #[cfg(unix)]
        let mut decoder = InputDecoder::default();
        #[cfg(not(unix))]
        let decoder = InputDecoder::default();
        #[cfg(unix)]
        if tty.is_some() {
            // The keyboard-enhancement probe ran before our status probe.
            // Preserve keys it queued before switching to the raw reader.
            while event::poll(Duration::ZERO)? {
                decoder.events.push_back(event::read()?);
            }
        }
        Ok(Self {
            #[cfg(unix)]
            tty,
            decoder,
            probe_enabled,
            started: Instant::now(),
            last_input: Instant::now(),
            expired: !probe_enabled,
            #[cfg(unix)]
            size: crossterm::terminal::size().ok(),
        })
    }

    /// Unix reads bytes; Windows receives VT replies as console key events.
    pub(super) const fn supports_probe() -> bool {
        cfg!(any(unix, windows))
    }

    pub(super) fn probe_enabled(&self) -> bool {
        self.probe_enabled
    }

    pub(super) fn read(
        &mut self,
        emitter: &mut ProgramStatusEmitter,
        timeout: Duration,
    ) -> io::Result<Option<Event>> {
        if self.probe_enabled {
            #[cfg(unix)]
            return self.read_raw(emitter, timeout);
            #[cfg(windows)]
            return self.read_console(emitter, timeout);
        }
        let _ = emitter;
        if event::poll(timeout)? {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    }

    fn ready_event(&mut self, emitter: &mut ProgramStatusEmitter) -> Option<Event> {
        if !self.expired && self.started.elapsed() >= PROBE_TIMEOUT {
            self.decoder.feed(&emitter.expire_stream_probe());
            self.expired = true;
        }
        // CSI/OSC introducers are ambiguous with Alt+[ / Alt+]. Give a
        // fragmented terminal reply the probe window before releasing those
        // keys; a bare Escape keeps the ordinary short ambiguity delay.
        let delay = if emitter.is_control_prefix() {
            PROBE_TIMEOUT
        } else {
            ESCAPE_DELAY
        };
        if self.last_input.elapsed() >= delay {
            self.decoder.feed(&emitter.release_key_prefix());
            self.decoder.parse(false);
        }
        self.decoder.events.pop_front()
    }

    #[cfg(windows)]
    fn read_console(
        &mut self,
        emitter: &mut ProgramStatusEmitter,
        timeout: Duration,
    ) -> io::Result<Option<Event>> {
        use event::{KeyCode, KeyEventKind, KeyModifiers};
        if let Some(event) = self.ready_event(emitter) {
            return Ok(Some(event));
        }
        if !event::poll(timeout.min(ESCAPE_DELAY))? {
            return Ok(None);
        }
        let input = event::read()?;
        // Console input represents VT query replies as individual key events.
        // Decode only unmodified ASCII/Escape presses; preserve native mouse,
        // resize, Unicode and modified-key events in their original form.
        let byte = match &input {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && (key.modifiers - KeyModifiers::SHIFT).is_empty() =>
            {
                match key.code {
                    KeyCode::Esc => Some(0x1b),
                    KeyCode::Char(c) if c.is_ascii() => Some(c as u8),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(byte) = byte {
            self.last_input = Instant::now();
            self.decoder.feed(&emitter.observe_stream_input(&[byte]));
            Ok(self.decoder.events.pop_front())
        } else {
            Ok(Some(input))
        }
    }

    #[cfg(unix)]
    fn read_raw(
        &mut self,
        emitter: &mut ProgramStatusEmitter,
        timeout: Duration,
    ) -> io::Result<Option<Event>> {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        use std::io::Read;
        use std::os::fd::AsFd;

        if let Some(event) = self.ready_event(emitter) {
            return Ok(Some(event));
        }
        // Query size on each tick; this avoids a second signal/input reader
        // and preserves resizing while there are no key presses.
        if let Ok(size) = crossterm::terminal::size() {
            if self.size.replace(size) != Some(size) {
                return Ok(Some(Event::Resize(size.0, size.1)));
            }
        }
        let tty = self.tty.as_mut().expect("raw reader has a tty");
        let ready = {
            let mut fds = [PollFd::new(tty.as_fd(), PollFlags::POLLIN)];
            match poll(
                &mut fds,
                PollTimeout::try_from(timeout.min(ESCAPE_DELAY)).unwrap_or(PollTimeout::ZERO),
            ) {
                Ok(n) => n > 0,
                Err(nix::errno::Errno::EINTR) => false,
                Err(error) => return Err(error.into()),
            }
        };
        if ready {
            let mut bytes = [0; 4096];
            match tty.read(&mut bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.last_input = Instant::now();
                    self.decoder
                        .feed(&emitter.observe_stream_input(&bytes[..n]));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(self.decoder.events.pop_front())
    }
}
