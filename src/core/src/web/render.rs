//! HTTP broker for a renderer with no ambient network access.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{future::BoxFuture, StreamExt};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
const MAX_RESOURCE: usize = 20 * 1024 * 1024;
const MAX_TRAFFIC: usize = 48 * 1024 * 1024;
const MAX_REQUESTS: usize = 100;
struct Broker<'a> {
    approvals: &'a Approvals,
    request_id: &'a str,
    emit: &'a (dyn Fn(AgentEvent) + Send + Sync),
    bytes: AtomicUsize,
    requests: AtomicUsize,
    failure: Mutex<Option<ToolError>>,
    #[cfg(test)]
    client: Option<reqwest::Client>,
}
impl Broker<'_> {
    async fn retrieve(&self, value: &str) -> Result<Resource, ToolError> {
        let url = checked_url(value)?;
        validate_literal_address(&url)?;
        if self.requests.fetch_add(1, Ordering::Relaxed) >= MAX_REQUESTS {
            return Err(WebError::new(
                WebErrorCategory::SizeLimit,
                &url,
                "Rendering exceeded 100 resource requests.",
            )
            .into());
        }
        if !self.approvals.request_web(self.request_id, url.host_str().unwrap_or_default(), format!("Load browser resource\nGET {}\n\nThis host receives a credential-free request for the rendered page. Each resource host and redirect uses your web permission rules. Scripts, frames and fetches may contact additional public hosts. Cookies, authentication, uploads, workers and WebSockets are disabled.", error::source_url(&url)), self.emit).await.map_err(ToolError::Execution)? { return Err(ToolError::Denied); }
        #[cfg(test)]
        let client = match &self.client {
            Some(client) => client.clone(),
            None => pinned_client(&url).await?,
        };
        #[cfg(not(test))]
        let client = pinned_client(&url).await?;
        let response = client
            .get(url.clone())
            .header(
                "User-Agent",
                "Blackwall/0.1 (+isolated public web rendering)",
            )
            .send()
            .await
            .map_err(|e| error::transport_error(&e, &url))?;
        let status = response.status();
        if response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|h| h == "challenge")
        {
            return Err(WebError::new(WebErrorCategory::Challenge, &url, "The rendered source requires human verification. Use another public source or open it in your browser; Blackwall cannot share that login.").into());
        }
        if status.is_redirection() {
            // Return the redirect to Chromium, so the next request independently checks host,
            // fresh DNS, permissions and the shared request bound. Never follow in reqwest.
            let mut visited = std::collections::HashSet::new();
            let next = redirect_destination(&url, response.headers(), 0, &mut visited)?;
            return Ok(Resource {
                status: status.as_u16(),
                content_type: "text/plain".into(),
                body: String::new(),
                location: Some(next.to_string()),
            });
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        if response.headers().contains_key("content-disposition") {
            return Err(WebError::new(
                WebErrorCategory::UnsupportedDocument,
                &url,
                "Browser downloads are disabled. Use a public text page.",
            )
            .into());
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| error::transport_error(&e, &url))?;
            let total = self.bytes.fetch_add(chunk.len(), Ordering::Relaxed);
            if chunk.len() > MAX_RESOURCE.saturating_sub(bytes.len())
                || chunk.len() > MAX_TRAFFIC.saturating_sub(total)
            {
                return Err(WebError::new(WebErrorCategory::SizeLimit, &url, "Rendering exceeded the 20 MiB resource or 48 MiB total decompressed traffic limit.").into());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Resource {
            status: status.as_u16(),
            content_type,
            body: STANDARD.encode(bytes),
            location: None,
        })
    }
}
impl RenderNetwork for Broker<'_> {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Resource, ToolError>> {
        Box::pin(async move {
            match self.retrieve(url).await {
                Ok(resource) => Ok(resource),
                Err(error) => {
                    if let Ok(mut slot) = self.failure.lock() {
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                    }
                    Err(ToolError::Denied) // No secret-bearing transport details cross into the browser.
                }
            }
        })
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    value: &str,
    purpose: &str,
    request_id: &str,
    approvals: &Approvals,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
    runtime: Option<&WebRuntime>,
    enabled: bool,
) -> Result<String, ToolError> {
    let url = checked_url(value)?;
    validate_literal_address(&url)?;
    if !enabled {
        let mut error = renderer_unavailable(value);
        error.message = "Browser rendering is turned off. Enable it in Settings → Web access, or use another public source.".into();
        return Err(error.into());
    }
    let renderer = runtime
        .and_then(|runtime| runtime.renderer.as_ref())
        .ok_or_else(|| renderer_unavailable(value))?;
    if !approvals.request_web(request_id, url.host_str().unwrap_or_default(), format!("Render public page\nGET {}\nPurpose: {purpose}\n\nExecute untrusted JavaScript in an isolated browser. Resource hosts and redirects each use your web permission rules. No browser login or ambient credentials are shared. Rendering is limited to 30 seconds, 100 requests and 48 MiB. A human-verification challenge will still require another source.", error::source_url(&url)), emit).await.map_err(ToolError::Execution)? { return Err(ToolError::Denied); }
    let broker = Broker {
        approvals,
        request_id,
        emit,
        bytes: AtomicUsize::new(0),
        requests: AtomicUsize::new(0),
        failure: Mutex::new(None),
        #[cfg(test)]
        client: None,
    };
    let result =
        tokio::time::timeout(Duration::from_secs(30), renderer.render(value, &broker)).await;
    if let Some(error) = broker.failure.lock().ok().and_then(|mut slot| slot.take()) {
        return Err(error);
    }
    let page = result.map_err(|_| {
        WebError::new(
            WebErrorCategory::Timeout,
            &url,
            "Browser rendering exceeded 30 seconds. Use another public source.",
        )
    })??;
    let final_url = checked_url(&page.url)?;
    validate_literal_address(&final_url)?;
    if page.html.len() > MAX_DOWNLOAD_BYTES {
        return Err(WebError::new(
            WebErrorCategory::SizeLimit,
            &final_url,
            "Rendered DOM exceeded 2 MiB.",
        )
        .into());
    }
    if page.html.to_ascii_lowercase().contains("type=\"password\"")
        || page.html.to_ascii_lowercase().contains("type='password'")
    {
        return Err(WebError::new(WebErrorCategory::Authentication, &final_url, "The rendered page requires login. Use another public source; opening it in your browser does not share its authenticated content with Blackwall.").into());
    }
    document::render(
        page.html.as_bytes(),
        "text/html; charset=utf-8",
        &final_url,
        false,
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
    use axum::{routing::get, Router};
    use std::{future::IntoFuture, sync::Arc};
    fn allow(approvals: &Approvals, event: AgentEvent) {
        if let AgentEvent::ApprovalRequest {
            request_id,
            approval_id,
            ..
        } = event
        {
            approvals
                .resolve(ResolveApprovalRequest {
                    request_id,
                    approval_id,
                    decision: ApprovalDecision::Allow,
                })
                .unwrap();
        }
    }
    #[tokio::test]
    async fn resource_broker_checks_redirects_hosts_credentials_and_traffic_bounds() {
        let seen = Arc::new(Mutex::new(vec![]));
        let observed = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/{*path}",
                    get(
                        move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                            let seen = observed.clone();
                            async move {
                                seen.lock().unwrap().push(headers);
                                match uri.path() {
                                    "/redirect" => axum::http::Response::builder()
                                        .status(302)
                                        .header("location", "http://127.0.0.1/metadata")
                                        .body("".into())
                                        .unwrap(),
                                    "/cross" => axum::http::Response::builder()
                                        .status(302)
                                        .header("location", "http://denied.test/frame")
                                        .body("".into())
                                        .unwrap(),
                                    _ => axum::http::Response::builder()
                                        .header("content-type", "text/html")
                                        .header("set-cookie", "ambient=secret")
                                        .body("public fixture".to_owned())
                                        .unwrap(),
                                }
                            }
                        },
                    ),
                ),
            )
            .into_future(),
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("fixture.test", address)
            .build()
            .unwrap();
        let approvals = Approvals::default();
        let emit = |event| allow(&approvals, event);
        let broker = Broker {
            approvals: &approvals,
            request_id: "render",
            emit: &emit,
            requests: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            failure: Mutex::new(None),
            client: Some(client),
        };
        for url in [
            "http://127.0.0.1/private",
            "http://[::ffff:127.0.0.1]/",
            "http://user:password@fixture.test/",
            "file:///etc/passwd",
            "ws://fixture.test/socket",
        ] {
            assert!(broker.retrieve(url).await.is_err());
        }
        assert!(seen.lock().unwrap().is_empty());
        broker.retrieve("http://fixture.test/one").await.unwrap();
        broker.retrieve("http://fixture.test/two").await.unwrap();
        for headers in seen.lock().unwrap().iter() {
            for forbidden in ["cookie", "authorization", "referer", "proxy-authorization"] {
                assert!(!headers.contains_key(forbidden));
            }
        }
        assert!(
            matches!(broker.retrieve("http://fixture.test/redirect").await, Err(ToolError::Web(e)) if e.category == WebErrorCategory::NetworkPolicy)
        );
        let cross = broker.retrieve("http://fixture.test/cross").await.unwrap();
        assert_eq!(cross.location.as_deref(), Some("http://denied.test/frame"));
        let deny = |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                approvals
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Deny,
                    })
                    .unwrap();
            }
        };
        let denied = Broker {
            approvals: &approvals,
            request_id: "deny",
            emit: &deny,
            requests: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            failure: Mutex::new(None),
            client: broker.client.clone(),
        };
        assert!(matches!(
            denied.retrieve(cross.location.as_deref().unwrap()).await,
            Err(ToolError::Denied)
        ));
        broker.bytes.store(MAX_TRAFFIC, Ordering::Relaxed);
        assert!(
            matches!(broker.retrieve("http://fixture.test/limit").await, Err(ToolError::Web(e)) if e.category == WebErrorCategory::SizeLimit)
        );
        broker.requests.store(MAX_REQUESTS, Ordering::Relaxed);
        assert!(
            matches!(broker.retrieve("http://fixture.test/limit").await, Err(ToolError::Web(e)) if e.category == WebErrorCategory::SizeLimit)
        );
        task.abort();
    }
    #[test]
    fn resource_dns_validation_refuses_mixed_and_rebound_addresses() {
        let url = Url::parse("https://example.com/").unwrap();
        let public = "1.1.1.1:443".parse().unwrap();
        let private = "10.0.0.1:443".parse().unwrap();
        assert!(validate_addresses(&url, &[public]).is_ok());
        assert!(validate_addresses(&url, &[public, private]).is_err());
        // The broker constructs a new pinned client on every resource/redirect request; a
        // later DNS answer never inherits validation from the first resolution.
        assert!(validate_addresses(&url, &[private]).is_err());
    }
}
