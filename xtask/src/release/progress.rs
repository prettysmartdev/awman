//! Operator-facing release progress. Only selected identifiers and bounded,
//! sanitized service status reach the terminal; diagnostics stay in phase logs.
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

fn line(message: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "release: {message}");
    let _ = out.flush();
}

pub fn safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(240)
        .collect()
}

pub fn service_label(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if value.len() > 80
        || ["token", "secret", "password", "credential"]
            .iter()
            .any(|word| lower.contains(word))
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')'))
    {
        "[redacted]".into()
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::service_label;

    #[test]
    fn status_labels_do_not_expose_paths_or_credentials() {
        assert_eq!(service_label("Run tests"), "Run tests");
        for value in [
            "/private/tmp/job",
            "token=abc123",
            "secret value",
            "line\nnext",
        ] {
            assert_eq!(service_label(value), "[redacted]");
        }
    }
}

pub struct Progress {
    phase: String,
    identity: String,
    started: Instant,
    state: Arc<Mutex<BTreeMap<String, String>>>,
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    ended: bool,
}

impl Progress {
    pub fn start(
        phase: &str,
        version: &str,
        commit: &str,
        log: &str,
        ledger: &str,
        resumed: bool,
    ) -> Self {
        let identity = format!("{version} @ {}", &commit[..commit.len().min(12)]);
        line(&format!(
            "{phase} {} {identity} (elapsed 0 s); log: {log}; timings: {ledger}",
            if resumed { "resumed" } else { "started" },
        ));
        let state = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));
        let (stop, receiver) = mpsc::channel();
        let background = Arc::clone(&state);
        let name = phase.to_string();
        let heartbeat_identity = identity.clone();
        let started = Instant::now();
        let thread = std::thread::spawn(move || {
            while receiver.recv_timeout(Duration::from_secs(30)).is_err() {
                let details = background
                    .lock()
                    .map(|s| s.values().cloned().collect::<Vec<_>>().join("; "))
                    .unwrap_or_default();
                line(&format!(
                    "{name} active {heartbeat_identity} (elapsed {} s){}",
                    started.elapsed().as_secs(),
                    if details.is_empty() {
                        String::new()
                    } else {
                        format!(": {details}")
                    }
                ));
            }
        });
        Self {
            phase: phase.into(),
            identity,
            started: Instant::now(),
            state,
            stop,
            thread: Some(thread),
            ended: false,
        }
    }

    pub fn update(&self, key: &str, detail: String) {
        let detail = safe(&detail);
        if let Ok(mut state) = self.state.lock() {
            if state.get(key) != Some(&detail) {
                state.insert(key.into(), detail.clone());
                line(&format!(
                    "{}: {detail} {} (elapsed {} s)",
                    self.phase,
                    self.identity,
                    self.started.elapsed().as_secs()
                ));
            }
        }
    }

    pub fn event(&self, detail: &str) {
        line(&format!(
            "{}: {} {} (elapsed {} s)",
            self.phase,
            safe(detail),
            self.identity,
            self.started.elapsed().as_secs()
        ));
    }

    pub fn end(mut self, outcome: &str) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        line(&format!(
            "{} {outcome} {} (elapsed {} s)",
            self.phase,
            self.identity,
            self.started.elapsed().as_secs()
        ));
        self.ended = true;
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if !self.ended {
            line(&format!(
                "{} blocked {} (elapsed {} s)",
                self.phase,
                self.identity,
                self.started.elapsed().as_secs()
            ));
        }
    }
}
