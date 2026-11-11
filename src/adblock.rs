//! Built-in ad and tracker blocking engine.
//!
//! Uses Aho-Corasick for O(n) multi-pattern substring matching against
//! request URLs, with an optional compiled regex set for complex rules.
//! This runs on the UI/network thread before any request reaches WebKit.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::Result;
use once_cell::sync::OnceCell;
use std::path::Path;
use tracing::info;

/// Compiled blocker — thread-safe after construction.
pub struct AdBlocker {
    ac: AhoCorasick,
    pattern_count: usize,
}

static EMBEDDED_PATTERNS: &[&str] = &[
    // Google Ads / DoubleClick
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "adservice.google.",
    // Facebook tracking
    "facebook.com/tr",
    "connect.facebook.net",
    // Common ad networks
    "ads.pubmatic.com",
    "ad.turn.com",
    "adsystem.amazon.",
    "pagead2.googlesyndication.com",
    "tpc.googlesyndication.com",
    // Analytics trackers
    "google-analytics.com",
    "analytics.google.com",
    "hotjar.com",
    "fullstory.com",
    "logrocket.com",
    // Social trackers
    "platform.twitter.com",
    "widgets.pinterest.com",
    "disqus.com/count",
    // Popunder / redirect networks
    "popcash.net",
    "popads.net",
    "trafficfactory.biz",
    // Telemetry / beacons
    "/beacon.js",
    "/track.js",
    "/pixel.gif",
    "/pixel.png",
    "telemetry.",
    "metrics.",
];

impl AdBlocker {
    /// Build the blocker from embedded defaults, optionally merging a
    /// custom blocklist file (one URL pattern per line, # = comment).
    pub fn new(custom_path: Option<&Path>) -> Result<Self> {
        let mut patterns: Vec<String> = EMBEDDED_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();

        if let Some(path) = custom_path {
            if path.exists() {
                let content = std::fs::read_to_string(path)?;
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !line.starts_with('#') {
                        patterns.push(line.to_string());
                    }
                }
                info!("Loaded {} custom blocklist patterns from {}",
                      patterns.len() - EMBEDDED_PATTERNS.len(),
                      path.display());
            }
        }

        let pattern_count = patterns.len();
        let ac = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostFirst)
            .ascii_case_insensitive(true)
            .build(&patterns)?;

        info!("AdBlocker initialized with {} patterns", pattern_count);
        Ok(Self { ac, pattern_count })
    }

    /// Returns `true` if the given URL should be blocked.
    #[inline]
    pub fn should_block(&self, url: &str) -> bool {
        self.ac.is_match(url)
    }

    pub fn pattern_count(&self) -> usize {
        self.pattern_count
    }
}

/// Global singleton — initialized once at startup.
static BLOCKER: OnceCell<AdBlocker> = OnceCell::new();

pub fn init_global_blocker(custom_path: Option<&Path>) -> Result<()> {
    let blocker = AdBlocker::new(custom_path)?;
    let count = blocker.pattern_count();
    if BLOCKER.set(blocker).is_ok() {
        info!("Global adblocker active ({} patterns)", count);
    }
    Ok(())
}

pub fn is_blocked(url: &str) -> bool {
    BLOCKER.get().map(|b| b.should_block(url)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_known_ad_and_tracker_domains() {
        let blocker = AdBlocker::new(None).expect("blocker init");
        assert!(blocker.pattern_count() >= 20);
        assert!(blocker.should_block("https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js"));
        assert!(blocker.should_block("https://www.google-analytics.com/analytics.js"));
        assert!(!blocker.should_block("https://www.rust-lang.org/"));
    }
}
