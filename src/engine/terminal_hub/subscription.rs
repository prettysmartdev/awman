//! One viewer's bounded stream. Resubscription and snapshots share the
//! registry lock with producers, so recovered output starts after the screen.
//!
//! A viewer that asks for history (WI 0105) first gets the retained raw
//! output as `History` frames, then the usual snapshot, all queued under the
//! same lock that creates the receiver: history, snapshot and live output
//! neither overlap nor leave a gap. Lag recovery never replays history.

use std::collections::VecDeque;
use std::sync::{Mutex, Weak};

use tokio::sync::broadcast;

use super::registry::{HubMessage, HubState, TerminalEntry};
use super::{SubscribeOptions, MAX_OUTPUT_FRAME_BYTES};
use crate::data::ids::TerminalId;
use crate::data::terminal::{TerminalFrame, TerminalSnapshot};

pub struct TerminalSubscription {
    id: TerminalId,
    /// At most one copy of the retained history (`ring_bytes`, split into
    /// frames of at most [`MAX_OUTPUT_FRAME_BYTES`]), a snapshot, state
    /// frames and an exit; independent of producer speed.
    pending: VecDeque<TerminalFrame>,
    rx: broadcast::Receiver<HubMessage>,
    state: Weak<Mutex<HubState>>,
    ended: bool,
}

impl std::fmt::Debug for TerminalSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSubscription")
            .field("id", &self.id)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

fn snapshot_frame(snapshot: TerminalSnapshot) -> TerminalFrame {
    TerminalFrame::Snapshot {
        has_output: snapshot.has_output,
        cols: snapshot.cols,
        rows: snapshot.rows,
        screen: snapshot.screen,
    }
}

/// The mirror's retained history as frames, oldest first. Never empty: a
/// history with nothing to replay is still one (empty) frame, so a viewer
/// can tell "nothing retained" (`truncated`) from "nothing emitted".
fn history_frames(entry: &TerminalEntry) -> impl Iterator<Item = TerminalFrame> + '_ {
    let history = entry.mirror.history();
    let chunks: Vec<&[u8]> = if history.bytes.is_empty() {
        vec![&[]]
    } else {
        history.bytes.chunks(MAX_OUTPUT_FRAME_BYTES).collect()
    };
    chunks
        .into_iter()
        .enumerate()
        .map(move |(index, bytes)| TerminalFrame::History {
            bytes: bytes.to_vec(),
            truncated: index == 0 && history.truncated,
        })
}

impl TerminalSubscription {
    pub(super) fn new(
        id: TerminalId,
        entry: &TerminalEntry,
        state: Weak<Mutex<HubState>>,
        options: SubscribeOptions,
    ) -> Self {
        let mut pending = VecDeque::new();
        if options.history {
            pending.extend(history_frames(entry));
        }
        pending.push_back(snapshot_frame(entry.mirror.snapshot()));
        if let Some(snapshot) = &entry.record.automation {
            pending.push_back(TerminalFrame::AutomationState {
                snapshot: Box::new(snapshot.clone()),
            });
        }
        if let Some(credentials) = &entry.record.credential_updates {
            pending.push_back(TerminalFrame::CredentialUpdateState {
                snapshot: Box::new(credentials.public_state()),
            });
        }
        // A close result published before finalization stays recoverable.
        if entry.output.is_some() {
            if let Some(frame) = &entry.close_result {
                pending.push_back((**frame).clone());
            }
        }
        let rx = if let Some(output) = &entry.output {
            output.subscribe()
        } else {
            if let Some(frame) = &entry.close_result {
                pending.push_back((**frame).clone());
            }
            pending.push_back(TerminalFrame::Exit {
                code: entry
                    .record
                    .exit_code
                    .expect("closed terminal has exit code"),
            });
            let (_, rx) = broadcast::channel(1);
            rx
        };
        Self {
            id,
            pending,
            rx,
            state,
            ended: false,
        }
    }

    pub fn id(&self) -> &TerminalId {
        &self.id
    }

    /// Snapshot first; lag is followed by an atomic fresh snapshot. Exit is
    /// returned once, then all calls return `None`, even for retained records.
    /// Cancellation while waiting for the next frame loses no frame.
    pub async fn next(&mut self) -> Option<TerminalFrame> {
        if self.ended {
            return None;
        }
        if let Some(frame) = self.pending.pop_front() {
            if matches!(frame, TerminalFrame::Exit { .. }) {
                self.ended = true;
            }
            return Some(frame);
        }
        match self.rx.recv().await {
            Ok(HubMessage::Frame(frame)) => Some((*frame).clone()),
            Ok(HubMessage::Finished { code, .. }) => {
                self.ended = true;
                Some(TerminalFrame::Exit { code })
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.ended = true;
                None
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                if self.recover() {
                    Some(TerminalFrame::Lagged)
                } else {
                    self.ended = true;
                    None
                }
            }
        }
    }

    fn recover(&mut self) -> bool {
        if let Some(state) = self.state.upgrade() {
            let state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Ok(entry) = state.entry(&self.id) {
                self.pending
                    .push_back(snapshot_frame(entry.mirror.snapshot()));
                if let Some(credentials) = &entry.record.credential_updates {
                    self.pending
                        .push_back(TerminalFrame::CredentialUpdateState {
                            snapshot: Box::new(credentials.public_state()),
                        });
                }
                if let Some(snapshot) = &entry.record.automation {
                    self.pending.push_back(TerminalFrame::AutomationState {
                        snapshot: Box::new(snapshot.clone()),
                    });
                }
                if entry.output.is_some() {
                    if let Some(frame) = &entry.close_result {
                        self.pending.push_back((**frame).clone());
                    }
                }
                if let Some(output) = &entry.output {
                    self.rx = output.subscribe();
                } else {
                    if let Some(frame) = &entry.close_result {
                        self.pending.push_back((**frame).clone());
                    }
                    self.pending.push_back(TerminalFrame::Exit {
                        code: entry
                            .record
                            .exit_code
                            .expect("closed terminal has exit code"),
                    });
                }
                return true;
            }
        }
        // Forgetting (or dropping the hub after exit) removes the mirror.
        // The bounded queue's last message still carries the final screen.
        loop {
            match self.rx.try_recv() {
                Ok(HubMessage::Finished {
                    code,
                    snapshot,
                    automation,
                    close_result,
                }) => {
                    self.pending.push_back(snapshot_frame((*snapshot).clone()));
                    if let Some(snapshot) = automation {
                        self.pending.push_back(TerminalFrame::AutomationState {
                            snapshot: Box::new((*snapshot).clone()),
                        });
                    }
                    if let Some(frame) = close_result {
                        self.pending.push_back((*frame).clone());
                    }
                    self.pending.push_back(TerminalFrame::Exit { code });
                    return true;
                }
                Ok(HubMessage::Frame(_)) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    }
}

#[cfg(test)]
mod history_tests {
    use std::sync::Arc;

    use crate::data::daemon_state::FeatureName;
    use crate::data::ids::TabId;
    use crate::data::terminal::{TerminalFrame, TerminalOwner, TerminalOwnerInfo};
    use crate::engine::terminal_hub::{
        input_channel, SubscribeOptions, TerminalHandle, TerminalHub, TerminalHubConfig,
        TerminalInput, TerminalRegistration, MAX_OUTPUT_FRAME_BYTES,
    };

    const HISTORY: SubscribeOptions = SubscribeOptions { history: true };

    fn hub(config: TerminalHubConfig) -> (Arc<TerminalHub>, TerminalHandle) {
        let hub = Arc::new(TerminalHub::new(config));
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
                cols: 40,
                rows: 4,
            },
            TerminalInput {
                stdin,
                resize: None,
            },
        );
        (hub, handle)
    }

    /// Frames already queued, without waiting for live output.
    fn queued(sub: &mut super::TerminalSubscription) -> Vec<TerminalFrame> {
        sub.pending.drain(..).collect()
    }

    fn history_bytes(frames: &[TerminalFrame]) -> (Vec<u8>, bool) {
        let mut bytes = Vec::new();
        let mut truncated = false;
        for (index, frame) in frames.iter().enumerate() {
            if let TerminalFrame::History {
                bytes: chunk,
                truncated: t,
            } = frame
            {
                assert!(!*t || index == 0, "only the first frame may be truncated");
                truncated |= t;
                bytes.extend_from_slice(chunk);
            }
        }
        (bytes, truncated)
    }

    #[tokio::test]
    async fn history_precedes_the_snapshot_and_live_output_follows() {
        let (hub, handle) = hub(TerminalHubConfig::default());
        for n in 0..5 {
            handle.push_output(format!("line {n}\r\n").as_bytes());
        }
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        let frames = queued(&mut sub);
        assert!(matches!(frames[0], TerminalFrame::History { .. }));
        assert!(matches!(
            frames.last(),
            Some(TerminalFrame::Snapshot {
                has_output: true,
                ..
            })
        ));
        let (bytes, truncated) = history_bytes(&frames);
        assert!(!truncated);
        assert_eq!(bytes, b"line 0\r\nline 1\r\nline 2\r\nline 3\r\nline 4\r\n");
        handle.push_output(b"live\r\n");
        assert_eq!(
            sub.next().await,
            Some(TerminalFrame::Output {
                bytes: b"live\r\n".to_vec()
            })
        );
    }

    #[tokio::test]
    async fn without_history_the_stream_is_unchanged() {
        let (hub, handle) = hub(TerminalHubConfig::default());
        handle.push_output(b"before\r\n");
        let mut sub = hub.subscribe(handle.id()).unwrap();
        let frames = queued(&mut sub);
        assert_eq!(frames.len(), 1);
        assert!(matches!(frames[0], TerminalFrame::Snapshot { .. }));
    }

    #[tokio::test]
    async fn history_is_split_into_bounded_frames_in_order() {
        let ring = 3 * MAX_OUTPUT_FRAME_BYTES + 100;
        let (hub, handle) = hub(TerminalHubConfig {
            ring_bytes: ring,
            ..Default::default()
        });
        let output: Vec<u8> = (0..ring).map(|i| b'a' + (i % 26) as u8).collect();
        handle.push_output(&output);
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        let frames = queued(&mut sub);
        let history: Vec<_> = frames
            .iter()
            .filter(|f| matches!(f, TerminalFrame::History { .. }))
            .collect();
        assert_eq!(history.len(), 4);
        for frame in &history {
            let TerminalFrame::History { bytes, .. } = frame else {
                unreachable!()
            };
            assert!(bytes.len() <= MAX_OUTPUT_FRAME_BYTES);
        }
        assert_eq!(history_bytes(&frames), (output, false));
    }

    #[tokio::test]
    async fn eviction_marks_the_first_frame_truncated_and_stays_bounded() {
        let (hub, handle) = hub(TerminalHubConfig {
            ring_bytes: 64,
            ..Default::default()
        });
        for n in 0..40 {
            handle.push_output(format!("line {n:02}\r\n").as_bytes());
        }
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        let (bytes, truncated) = history_bytes(&queued(&mut sub));
        assert!(truncated);
        assert!(bytes.len() <= 64);
        // Starts on a line boundary and ends with the newest line.
        assert!(bytes.starts_with(b"line "));
        assert!(bytes.ends_with(b"line 39\r\n"));
    }

    #[tokio::test]
    async fn concurrent_output_is_neither_lost_nor_duplicated() {
        let (hub, handle) = hub(TerminalHubConfig {
            subscriber_capacity: 4096,
            ..Default::default()
        });
        let producer = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                for n in 0..2000 {
                    handle.push_output(format!("{n}\n").as_bytes());
                }
            })
        };
        std::thread::yield_now();
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        producer.join().unwrap();
        handle.exit(0);
        let mut seen = Vec::new();
        while let Some(frame) = sub.next().await {
            match frame {
                TerminalFrame::History { bytes, .. } | TerminalFrame::Output { bytes } => {
                    seen.extend_from_slice(&bytes)
                }
                TerminalFrame::Exit { .. } => break,
                TerminalFrame::Snapshot { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        let expected: String = (0..2000).map(|n| format!("{n}\n")).collect();
        assert_eq!(String::from_utf8(seen).unwrap(), expected);
    }

    #[tokio::test]
    async fn lag_recovery_sends_a_snapshot_and_no_history() {
        let (hub, handle) = hub(TerminalHubConfig {
            subscriber_capacity: 1,
            ..Default::default()
        });
        handle.push_output(b"before\r\n");
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        while !matches!(sub.next().await, Some(TerminalFrame::Snapshot { .. })) {}
        for _ in 0..4 {
            handle.push_output(b"flood\r\n");
        }
        assert_eq!(sub.next().await, Some(TerminalFrame::Lagged));
        assert!(matches!(
            sub.next().await,
            Some(TerminalFrame::Snapshot { .. })
        ));
        assert!(!sub
            .pending
            .iter()
            .any(|f| matches!(f, TerminalFrame::History { .. })));
    }

    #[tokio::test]
    async fn finished_terminal_replays_history_then_snapshot_then_exit() {
        let (hub, handle) = hub(TerminalHubConfig::default());
        handle.push_output(b"final words\r\n");
        handle.exit(3);
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        assert_eq!(
            sub.next().await,
            Some(TerminalFrame::History {
                bytes: b"final words\r\n".to_vec(),
                truncated: false
            })
        );
        assert!(matches!(
            sub.next().await,
            Some(TerminalFrame::Snapshot { .. })
        ));
        assert_eq!(sub.next().await, Some(TerminalFrame::Exit { code: 3 }));
        assert_eq!(sub.next().await, None);
    }

    #[tokio::test]
    async fn empty_terminal_sends_one_empty_untruncated_history_frame() {
        let (hub, handle) = hub(TerminalHubConfig::default());
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        let frames = queued(&mut sub);
        assert_eq!(
            frames[0],
            TerminalFrame::History {
                bytes: Vec::new(),
                truncated: false
            }
        );
        assert!(matches!(
            frames[1],
            TerminalFrame::Snapshot {
                has_output: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn alternate_screen_with_evicted_history_is_one_empty_truncated_frame() {
        let (hub, handle) = hub(TerminalHubConfig {
            ring_bytes: 32,
            ..Default::default()
        });
        handle.push_output(b"\x1b[?1049h");
        handle.push_output(&[b'#'; 64]);
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        let frames = queued(&mut sub);
        assert_eq!(
            frames[0],
            TerminalFrame::History {
                bytes: Vec::new(),
                truncated: true
            }
        );
        assert!(matches!(frames[1], TerminalFrame::Snapshot { .. }));
    }

    #[tokio::test]
    async fn resize_never_resends_history() {
        let (hub, handle) = hub(TerminalHubConfig::default());
        handle.push_output(b"before\r\n");
        let mut sub = hub.subscribe_with(handle.id(), HISTORY).unwrap();
        queued(&mut sub);
        hub.resize(handle.id(), 60, 10).unwrap();
        assert_eq!(
            sub.next().await,
            Some(TerminalFrame::Resize { cols: 60, rows: 10 })
        );
        handle.push_output(b"after\r\n");
        assert!(matches!(
            sub.next().await,
            Some(TerminalFrame::Output { .. })
        ));
    }
}
