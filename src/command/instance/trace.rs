//! Opt-in engine-side conformance recording. Never records the environment.
//! Available only to test harnesses, never persisted or sent to a frontend.
use std::sync::Mutex;
#[derive(Default)]
pub struct OutcomeTrace(Mutex<Vec<serde_json::Value>>);
impl OutcomeTrace {
    pub fn record(&self, kind: &str, value: impl serde::Serialize) {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::json!({"kind":kind,"value":value}));
    }
    pub fn snapshot(&self) -> Vec<serde_json::Value> {
        self.0.lock().unwrap().clone()
    }
}
