//! Backoff/header parsing adapted from FlyCockpit's Cockpit, without daemon probes.
use std::time::{Duration, SystemTime};
pub const MAX_RETRIES: usize = 8;
pub fn retry_after(value: &str) -> Option<u64> {
    let value = value.trim_matches([' ', '\t']);
    let seconds = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        value.parse().ok()
    } else {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(SystemTime::now())
            .ok()
            .map(|d| d.as_secs())
    };
    seconds.map(|n| n.min(300))
}
pub fn backoff(attempt: usize) -> Duration {
    // Keep 500ms as the first delay; later waits carry Cockpit's downward jitter.
    let cap_ms = 500u64
        .saturating_mul(1u64.checked_shl(attempt as u32).unwrap_or(u64::MAX))
        .min(30_000);
    let jitter = if attempt == 0 {
        1.0
    } else {
        0.5 + (uuid::Uuid::new_v4().as_u128() % 1001) as f64 / 2000.0
    };
    Duration::from_millis((cap_ms as f64 * jitter).round() as u64)
}

/// Probe the effective proxy (when configured), otherwise the provider. No
/// credentials enter diagnostics. The caller races this future with backoff.
pub(crate) async fn reconnected(base: &str) {
    let Ok(url) = url::Url::parse(base) else {
        return std::future::pending().await;
    };
    let Some(host) = url.host_str() else {
        return std::future::pending().await;
    };
    let bypass = ["NO_PROXY", "no_proxy"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .any(|s| {
            s.split(',').any(|p| {
                let p = p.trim();
                p == "*"
                    || host == p
                    || (!p.is_empty() && host.ends_with(&format!(".{}", p.trim_start_matches('.'))))
            })
        });
    let proxy = if bypass {
        None
    } else {
        let keys: &[&str] = if url.scheme() == "https" {
            &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        } else {
            &["http_proxy", "HTTP_PROXY", "ALL_PROXY", "all_proxy"]
        };
        keys.iter()
            .find_map(|k| std::env::var(k).ok())
            .and_then(|s| url::Url::parse(&s).ok())
    };
    let target = proxy.as_ref().unwrap_or(&url);
    let host = target.host_str().unwrap_or(host).to_owned();
    let port = target.port_or_known_default().unwrap_or(443);
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if matches!(
            tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::TcpStream::connect((host.as_str(), port))
            )
            .await,
            Ok(Ok(_))
        ) {
            return;
        }
    }
}
