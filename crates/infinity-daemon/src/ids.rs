//! Unique-id generation for user-visible entities (sessions and threads).
//!
//! Production code uses random v4 UUIDs ([`UuidIdSource`]); tests inject a
//! deterministic sequence ([`SequentialIdSource`]) so ids rendered in UIs
//! (session ids in the sidebar/footer, thread ids in headers) are stable
//! across runs, which keeps screen and screenshot snapshots byte-identical.
//!
//! Holders share a source via `Arc<dyn IdSource>` so a single sequential
//! counter is used for every id in a test daemon.

use std::sync::atomic::{AtomicU64, Ordering};

/// A source of unique ids.
pub trait IdSource: Send + Sync {
    /// Generate the next id.
    fn generate(&self) -> String;
}

/// Random v4 UUIDs — the production default.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidIdSource;

impl IdSource for UuidIdSource {
    fn generate(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// Deterministic UUID-shaped sequence for tests:
/// `00000000-0000-4000-8000-000000000001`, `…-000000000002`, …
#[derive(Debug, Default)]
pub struct SequentialIdSource {
    counter: AtomicU64,
}

impl SequentialIdSource {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdSource for SequentialIdSource {
    fn generate(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("00000000-0000-4000-8000-{n:012x}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn sequential_ids_are_stable_and_shared() {
        let ids: Arc<dyn IdSource> = Arc::new(SequentialIdSource::new());
        assert_eq!(ids.generate(), "00000000-0000-4000-8000-000000000001");
        let shared = Arc::clone(&ids);
        assert_eq!(shared.generate(), "00000000-0000-4000-8000-000000000002");
        assert_eq!(ids.generate(), "00000000-0000-4000-8000-000000000003");
    }

    #[test]
    fn uuid_ids_are_unique() {
        let ids = UuidIdSource;
        assert_ne!(ids.generate(), ids.generate());
    }
}
