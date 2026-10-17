//! Event sink abstraction so backends can be built without a real `AppHandle` in tests.

use tauri::{AppHandle, Emitter};

/// Abstraction over the Tauri event sink so a backend can be built without a
/// real [`AppHandle`] in tests. Production uses [`AppHandle`]; tests use a
/// channel- or no-op sink. Payloads are passed as `serde_json::Value` so the
/// single trait method covers every event shape a backend emits.
pub trait EmitSink: Send + Sync {
    fn emit(&self, event: &str, payload: serde_json::Value);
}

impl EmitSink for AppHandle {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        let _ = Emitter::emit(self, event, payload);
    }
}
