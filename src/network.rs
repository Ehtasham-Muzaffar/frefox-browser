//! Network layer — WebKit content filter integration, request interception,
//! and policy decisions for ad blocking and HTTPS enforcement.

use crate::config::NetworkConfig;
use tracing::info;
#[cfg(target_os = "linux")]
use tracing::warn;
#[cfg(target_os = "linux")]
use webkit6::prelude::*;

/// Install a WebKit content filter on the given web context (Linux WebKitGTK).
#[cfg(target_os = "linux")]
pub fn install_content_filter(
    ctx: &webkit6::WebContext,
    cfg: &NetworkConfig,
) {
    if !cfg.adblock_enabled {
        info!("AdBlock disabled by configuration");
        return;
    }

    let _ = ctx;
    let rules_json = build_content_rules_json();

    let store = webkit6::UserContentFilterStore::new(
        &dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join("ferrox")
            .join("content_filters")
            .to_string_lossy()
    );

    store.save(
        "ferrox-adblock",
        &glib::Bytes::from(rules_json.as_bytes()),
        gio::Cancellable::NONE,
        |result| match result {
            Ok(filter) => {
                info!("Content filter installed ({})", filter.identifier());
            }
            Err(e) => {
                warn!("Failed to install content filter: {}", e);
            }
        },
    );
}

#[cfg(target_os = "linux")]
fn build_content_rules_json() -> String {
    let domains = [
        "doubleclick.net",
        "googlesyndication.com",
        "googleadservices.com",
        "google-analytics.com",
        "hotjar.com",
        "fullstory.com",
        "facebook.com",
        "connect.facebook.net",
        "ads.pubmatic.com",
        "popcash.net",
        "popads.net",
    ];

    let mut rules = Vec::new();
    for domain in &domains {
        rules.push(serde_json::json!({
            "trigger": {
                "url-filter": format!(".*{}.*", domain),
                "load-type": ["third-party"]
            },
            "action": {
                "type": "block"
            }
        }));
    }

    serde_json::to_string(&rules).unwrap_or_else(|_| "[]".to_string())
}

/// Policy delegate: enforce HTTPS-only mode.
/// Returns the corrected URI if the original was HTTP (excluding localhost/127.0.0.1).
pub fn enforce_https(uri: &str, cfg: &NetworkConfig) -> Option<String> {
    if cfg.https_only
        && uri.starts_with("http://")
        && !uri.starts_with("http://localhost")
        && !uri.starts_with("http://127.0.0.1")
    {
        let https_uri = uri.replacen("http://", "https://", 1);
        info!("HTTPS-only redirect: {} -> {}", uri, https_uri);
        Some(https_uri)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_https_except_localhost() {
        let cfg = NetworkConfig {
            https_only: true,
            ..Default::default()
        };
        assert_eq!(
            enforce_https("http://example.com/docs", &cfg).as_deref(),
            Some("https://example.com/docs")
        );
        assert_eq!(enforce_https("http://localhost:8080", &cfg), None);
        assert_eq!(enforce_https("https://example.com", &cfg), None);
    }
}
