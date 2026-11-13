use reqwest::Url;
use serde::Serialize;
use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebErrorCategory {
    InvalidUrl,
    NetworkPolicy,
    Dns,
    Tls,
    Connectivity,
    Timeout,
    RedirectLimit,
    RedirectLoop,
    SiteDenied,
    Authentication,
    RateLimited,
    Http,
    Challenge,
    UnsupportedDocument,
    Encoding,
    SizeLimit,
    RenderingRequired,
    SearchUnavailable,
    SearchAuthentication,
    SearchQuota,
    RendererUnavailable,
}

impl WebErrorCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid_url",
            Self::NetworkPolicy => "network_policy",
            Self::Dns => "dns",
            Self::Tls => "tls",
            Self::Connectivity => "connectivity",
            Self::Timeout => "timeout",
            Self::RedirectLimit => "redirect_limit",
            Self::RedirectLoop => "redirect_loop",
            Self::SiteDenied => "site_denied",
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::Http => "http",
            Self::Challenge => "challenge",
            Self::UnsupportedDocument => "unsupported_document",
            Self::Encoding => "encoding",
            Self::SizeLimit => "size_limit",
            Self::RenderingRequired => "rendering_required",
            Self::SearchUnavailable => "search_unavailable",
            Self::SearchAuthentication => "search_authentication",
            Self::SearchQuota => "search_quota",
            Self::RendererUnavailable => "renderer_unavailable",
        }
    }
}

/// Stable, sanitized diagnostics shared by the model, CLI and UI through tool output.
#[derive(Debug, Serialize)]
pub struct WebError {
    pub category: WebErrorCategory,
    pub source: String,
    pub message: String,
}

impl WebError {
    pub(super) fn new(category: WebErrorCategory, url: &Url, message: &str) -> Self {
        Self {
            category,
            source: source_url(url),
            message: message.into(),
        }
    }
}

impl fmt::Display for WebError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", serde_json::json!({"webError": self}))
    }
}
impl Error for WebError {}

/// Queries and fragments can contain credentials. Never echo them in diagnostics or sources.
pub(super) fn source_url(url: &Url) -> String {
    let mut source = url.clone();
    let _ = source.set_username("");
    let _ = source.set_password(None);
    source.set_query(None);
    source.set_fragment(None);
    source.to_string()
}

pub(super) fn transport_error(error: &reqwest::Error, url: &Url) -> WebError {
    // Inspect the cause chain for classification only: reqwest's display includes request URLs.
    let mut cause: Option<&(dyn Error + 'static)> = error.source();
    let mut tls = false;
    while let Some(current) = cause {
        let detail = current.to_string().to_ascii_lowercase();
        tls |= ["certificate", "tls", "ssl", "invalid peer"]
            .iter()
            .any(|marker| detail.contains(marker));
        cause = current.source();
    }
    let (category, message) = if error.is_timeout() {
        (
            WebErrorCategory::Timeout,
            "The website timed out. Try again later or use another source.",
        )
    } else if tls {
        (WebErrorCategory::Tls, "The website's TLS connection could not be verified. Check the site's certificate and your network; certificate checks remain enabled.")
    } else if error.is_decode() {
        (
            WebErrorCategory::Encoding,
            "The website sent invalid compressed content. Try another source.",
        )
    } else {
        (WebErrorCategory::Connectivity, "The connection failed or was interrupted. Check your network and try another source. Web reads use a direct connection; proxy-only networks are unsupported.")
    };
    WebError::new(category, url, message)
}
