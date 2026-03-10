//! In-memory chat history storage, keyed by bearer token hash.
//!
//! Each paired client has an independent message history that persists
//! for the lifetime of the daemon process.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// A single history entry.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub id: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

impl HistoryEntry {
    pub fn new(role: &str, content: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            role: role.to_string(),
            name: None,
            content: content.to_string(),
            timestamp: Utc::now(),
        }
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }
}

/// Token-scoped chat history store.
///
/// Keys are SHA-256 hashes of bearer tokens (same format stored in
/// `paired_tokens`), so raw tokens are never held in memory.
#[derive(Debug, Clone, Default)]
pub struct ChatHistory {
    /// token_hash -> ordered message list
    store: Arc<Mutex<HashMap<String, Vec<HistoryEntry>>>>,
}

impl ChatHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an entry for the given token hash.
    pub fn push(&self, token_hash: &str, entry: HistoryEntry) {
        self.store
            .lock()
            .entry(token_hash.to_string())
            .or_default()
            .push(entry);
    }

    /// Return all entries for the given token hash.
    pub fn get(&self, token_hash: &str) -> Vec<HistoryEntry> {
        self.store
            .lock()
            .get(token_hash)
            .cloned()
            .unwrap_or_default()
    }

    /// Clear history for the given token hash.
    pub fn clear(&self, token_hash: &str) {
        self.store.lock().remove(token_hash);
    }
}
