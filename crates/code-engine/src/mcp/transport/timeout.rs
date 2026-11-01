use std::error::Error as _;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;

const MAX_REDIRECTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpTimeouts {
    pub connect: Duration,
    pub request: Duration,
}

impl McpTimeouts {
    pub fn from_secs(connect_timeout_secs: u64, timeout_secs: u64) -> Self {
        Self {
            connect: Duration::from_secs(connect_timeout_secs),
            request: Duration::from_secs(timeout_secs),
        }
    }
}

pub fn client(timeouts: McpTimeouts, endpoint: &str) -> Result<reqwest::Client> {
    let endpoint = validate_remote_endpoint(endpoint)?;
    let redirect_origin = endpoint.clone();
    reqwest::Client::builder()
        // Manager deadlines bound initialization/calls; an SSE reader has no total lifetime timeout.
        .connect_timeout(timeouts.connect.min(timeouts.request))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let target = attempt.url().clone();
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error(std::io::Error::other(format!(
                    "MCP redirect chain exceeded {MAX_REDIRECTS} redirects before {target}"
                )));
            }
            if !same_origin(&redirect_origin, &target) {
                return attempt.error(std::io::Error::other(format!(
                    "refusing MCP redirect to different origin or scheme: {target}"
                )));
            }
            attempt.follow()
        }))
        .build()
        .context("building MCP HTTP client")
}

pub(crate) fn validate_remote_endpoint(endpoint: &str) -> Result<Url> {
    let url = Url::parse(endpoint).with_context(|| format!("invalid MCP endpoint `{endpoint}`"))?;
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback(&url) => Ok(url),
        "http" => bail!(
            "refusing plaintext MCP endpoint `{url}`; use HTTPS (HTTP is allowed only for localhost, 127.0.0.1, or [::1])"
        ),
        scheme => bail!("unsupported MCP endpoint scheme `{scheme}` for `{url}`; use HTTPS"),
    }
}

fn is_loopback(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
    )
}

pub(crate) fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

pub fn timeout_error(label: &str, error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("{label} timed out")
    } else {
        anyhow::anyhow!(
            "{label}: {}",
            error
                .source()
                .map(ToString::to_string)
                .unwrap_or_else(|| error.to_string())
        )
    }
}
