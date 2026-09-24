//! Explicitly approved public web reads with DNS pinning, redirect validation, and bounds.
use crate::{
    approvals::Approvals,
    protocol::{AgentEvent, ApprovalKind},
    tools::ToolError,
};
use futures_util::StreamExt;
use reqwest::{StatusCode, Url};
use std::{net::IpAddr, time::Duration};

pub fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && (b == 0 || b == 88)))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_address(IpAddr::V4(v4));
            }
            // Only global unicast, excluding documentation, transition, and special-use blocks.
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && s[0] != 0x3fff
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
        }
    }
}
fn checked_url(value: &str) -> Result<Url, ToolError> {
    let url = Url::parse(value).map_err(|_| ToolError::Arguments)?;
    if value.len() > 4096
        || !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || !matches!(url.port_or_known_default(), Some(80 | 443))
    {
        return Err(ToolError::Execution("Web tools support public HTTP/HTTPS pages on standard ports, without embedded credentials.".into()));
    }
    Ok(url)
}
async fn pinned_client(url: &Url) -> Result<reqwest::Client, ToolError> {
    let host = url.host_str().ok_or(ToolError::Arguments)?;
    let port = url.port_or_known_default().ok_or(ToolError::Arguments)?;
    let addresses = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| ToolError::Execution("The website address could not be resolved in time.".into()))?
    .map_err(|_| ToolError::Execution("The website address could not be resolved.".into()))?
    .collect::<Vec<_>>();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !public_address(address.ip()))
    {
        return Err(ToolError::Execution(
            "Web tools cannot access local, private, or reserved network addresses.".into(),
        ));
    }
    reqwest::Client::builder()
        .no_proxy()
        .resolve_to_addrs(host, &addresses)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| ToolError::Execution("The web client could not start.".into()))
}

pub async fn fetch(
    value: &str,
    purpose: &str,
    request_id: &str,
    approvals: &Approvals,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
) -> Result<String, ToolError> {
    let mut url = checked_url(value)?;
    for redirects in 0..=2 {
        let detail=format!("Read web page\nGET {url}\nPurpose: {purpose}\n\nThe website receives your request and network address. Page contents are treated as untrusted information.");
        if !approvals
            .request(request_id, ApprovalKind::Network, detail, emit)
            .await
            .map_err(ToolError::Execution)?
        {
            return Err(ToolError::Denied);
        }
        let client = pinned_client(&url).await?;
        let response = client
            .get(url.clone())
            .header("User-Agent", "Blackwall/0.1")
            .send()
            .await
            .map_err(|_| ToolError::Execution("The website could not be reached.".into()))?;
        if matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            if redirects == 2 {
                return Err(ToolError::Execution(
                    "This page redirects too many times.".into(),
                ));
            }
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(ToolError::Arguments)?;
            url = checked_url(url.join(next).map_err(|_| ToolError::Arguments)?.as_str())?;
            continue;
        }
        if !response.status().is_success() {
            return Err(ToolError::Execution(format!(
                "The website returned HTTP {}.",
                response.status().as_u16()
            )));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !(content_type.starts_with("text/") || content_type.starts_with("application/json")) {
            return Err(ToolError::Execution(
                "This page is not a supported text document.".into(),
            ));
        }
        let mut bytes = vec![];
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| {
                ToolError::Execution("The website response was interrupted.".into())
            })?;
            if bytes.len() + chunk.len() > 256 * 1024 {
                return Err(ToolError::Execution(
                    "The page exceeded the 256 KiB download limit.".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            ToolError::Execution("The page uses an unsupported text encoding.".into())
        })?;
        let clipped: String = text.chars().take(32000).collect();
        return Ok(format!(
            "Source: {url}\nUntrusted page content (up to 32,000 characters):\n{clipped}"
        ));
    }
    Err(ToolError::Arguments)
}
pub fn search_url(query: &str) -> Result<String, ToolError> {
    if query.trim().is_empty() || query.len() > 1000 {
        return Err(ToolError::Arguments);
    }
    let mut url =
        Url::parse("https://html.duckduckgo.com/html/").map_err(|_| ToolError::Arguments)?;
    url.query_pairs_mut().append_pair("q", query);
    Ok(url.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    #[test]
    fn reserved_and_private_addresses_are_blocked() {
        for ip in [
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(100, 64, 0, 1),
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(198, 18, 0, 1),
            Ipv4Addr::new(192, 0, 2, 1),
        ] {
            assert!(!public_address(IpAddr::V4(ip)));
        }
        assert!(public_address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!public_address(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
        assert!(!public_address(IpAddr::V6(
            Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped()
        )));
    }
}
