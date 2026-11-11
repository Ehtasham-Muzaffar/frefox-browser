//! Lightweight Bookmark & History persistence for the Omnibox.
//!
//! Stores bookmarks and browsing history in compact JSON files under
//! `~/.local/share/ferrox/` to avoid heavy database engines in RAM.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub title: String,
    pub url: String,
    pub visited_at: DateTime<Utc>,
    pub visit_count: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreData {
    bookmarks: Vec<Bookmark>,
    history: Vec<HistoryEntry>,
}

/// Manages bookmarks and history with bounded in-memory retention.
pub struct BrowserStorage {
    data_path: PathBuf,
    data: StoreData,
    max_history: usize,
}

impl BrowserStorage {
    pub fn load() -> Self {
        let data_path = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("ferrox")
            .join("user_data.json");

        let data = if data_path.exists() {
            match std::fs::read_to_string(&data_path)
                .ok()
                .and_then(|s| serde_json::from_str::<StoreData>(&s).ok())
            {
                Some(d) => d,
                None => {
                    warn!("Corrupt user_data.json; starting with empty store");
                    StoreData::default()
                }
            }
        } else {
            StoreData::default()
        };

        Self {
            data_path,
            data,
            max_history: 2000,
        }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.data_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(&self.data)?;
        std::fs::write(&self.data_path, json)?;
        Ok(())
    }

    /// Record a visited URL in history, keeping the list capped to `max_history`.
    pub fn record_visit(&mut self, url: &str, title: &str) {
        if url.starts_with("about:") {
            return;
        }
        if let Some(existing) = self.data.history.iter_mut().find(|h| h.url == url) {
            existing.visit_count = existing.visit_count.saturating_add(1);
            existing.visited_at = Utc::now();
            if !title.is_empty() {
                existing.title = title.to_string();
            }
        } else {
            self.data.history.push(HistoryEntry {
                title: if title.is_empty() { url.to_string() } else { title.to_string() },
                url: url.to_string(),
                visited_at: Utc::now(),
                visit_count: 1,
            });
            if self.data.history.len() > self.max_history {
                self.data.history.remove(0);
            }
        }
        let _ = self.save();
    }

    /// Toggle bookmark state for `url`. Returns `true` if now bookmarked.
    pub fn toggle_bookmark(&mut self, url: &str, title: &str) -> bool {
        if let Some(pos) = self.data.bookmarks.iter().position(|b| b.url == url) {
            self.data.bookmarks.remove(pos);
            let _ = self.save();
            false
        } else {
            self.data.bookmarks.push(Bookmark {
                title: if title.is_empty() { url.to_string() } else { title.to_string() },
                url: url.to_string(),
                added_at: Utc::now(),
            });
            let _ = self.save();
            true
        }
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.data.bookmarks.iter().any(|b| b.url == url)
    }

    pub fn bookmarks(&self) -> &[Bookmark] {
        &self.data.bookmarks
    }

    pub fn recent_history(&self, limit: usize) -> Vec<&HistoryEntry> {
        let mut items: Vec<&HistoryEntry> = self.data.history.iter().collect();
        items.sort_by(|a, b| b.visited_at.cmp(&a.visited_at));
        items.truncate(limit);
        items
    }

    #[allow(dead_code)]
    pub fn clear_history(&mut self) {
        self.data.history.clear();
        let _ = self.save();
    }

    /// Return up to `limit` Omnibox suggestions matching `query` across bookmarks and history.
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<(String, String)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }

        let mut results: Vec<(String, String)> = Vec::new();

        for b in &self.data.bookmarks {
            if b.url.to_lowercase().contains(&q) || b.title.to_lowercase().contains(&q) {
                results.push((format!("★ {}", b.title), b.url.clone()));
            }
        }

        let mut hist_matches: Vec<&HistoryEntry> = self
            .data
            .history
            .iter()
            .filter(|h| h.url.to_lowercase().contains(&q) || h.title.to_lowercase().contains(&q))
            .collect();
        hist_matches.sort_by(|a, b| b.visit_count.cmp(&a.visit_count));

        for h in hist_matches {
            if !results.iter().any(|(_, u)| u == &h.url) {
                results.push((h.title.clone(), h.url.clone()));
            }
            if results.len() >= limit {
                break;
            }
        }

        results.truncate(limit);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_and_omnibox_suggestions_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = BrowserStorage {
            data_path: dir.path().join("user_data.json"),
            data: StoreData::default(),
            max_history: 100,
        };
        store.record_visit("https://doc.rust-lang.org/book/", "The Rust Book");
        assert!(store.toggle_bookmark("https://www.rust-lang.org/", "Rust"));
        assert!(store.is_bookmarked("https://www.rust-lang.org/"));

        let matches = store.suggest("rust", 5);
        assert_eq!(matches.len(), 2);
        assert!(matches[0].0.starts_with("★ "));
        assert_eq!(store.recent_history(5).len(), 1);
        store.clear_history();
        assert!(store.recent_history(5).is_empty());
    }
}

