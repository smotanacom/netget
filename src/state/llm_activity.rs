//! Live LLM work, scoped to the lifetime of the generation future.
//!
//! Unlike the conversation history, this excludes scripts and completed calls.
//! A weak registration disappears on success, error, or cancellation, without
//! needing async cleanup from Drop. Display snapshots never keep work alive.

use std::sync::{Arc, Mutex, Weak};

use super::app_state::ConversationSource;
use crate::utils::clock::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct LlmActivity {
    pub source: ConversationSource,
    pub details: String,
    pub started_at: Instant,
}

#[derive(Clone, Default)]
pub struct LlmActivityTracker {
    active: Arc<Mutex<Vec<Weak<LlmActivity>>>>,
}

impl LlmActivityTracker {
    /// Keep the returned handle in the generation future, until it finishes.
    pub fn begin(&self, source: ConversationSource, details: String) -> Arc<LlmActivity> {
        let activity = Arc::new(LlmActivity {
            source,
            details,
            started_at: Instant::now(),
        });
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.retain(|entry| entry.strong_count() > 0);
        active.push(Arc::downgrade(&activity));
        activity
    }

    pub fn snapshot(&self) -> Vec<LlmActivity> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = Vec::with_capacity(active.len());
        active.retain(|entry| match entry.upgrade() {
            Some(activity) => {
                snapshot.push((*activity).clone());
                true
            }
            None => false,
        });
        snapshot
    }
}
