//! Fixed-origin, credential-bound API search. No redirects, retries or implicit disclosure.
use super::*;
use futures_util::{future::BoxFuture, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Instant;

const BRAVE: &str = "https://api.search.brave.com/res/v1/web/search";
static BRAVE_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
static RETRY_AT: Mutex<Option<Instant>> = Mutex::new(None);
#[derive(Debug, Deserialize, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
pub trait SearchProvider: Send + Sync {
    fn search<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Result<Vec<SearchResult>, WebError>>;
}
pub struct BraveSearch {
    key: String,
}
impl BraveSearch {
    pub fn new(key: String) -> Self {
        Self { key }
    }
    pub async fn check(&self) -> Result<(), WebError> {
        self.search("Blackwall connection check").await.map(|_| ())
    }
}
fn failure(category: WebErrorCategory, message: &str) -> WebError {
    WebError {
        category,
        source: BRAVE.into(),
        message: message.into(),
    }
}
impl SearchProvider for BraveSearch {
    fn search<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Result<Vec<SearchResult>, WebError>> {
        Box::pin(async move {
            if query.trim().is_empty() || query.len() > 1000 {
                return Err(failure(
                    WebErrorCategory::SearchUnavailable,
                    "Use a search query between 1 and 1,000 bytes.",
                ));
            }
            let _slot = BRAVE_SLOT.acquire().await.map_err(|_| {
                failure(
                    WebErrorCategory::SearchUnavailable,
                    "Search is unavailable.",
                )
            })?;
            if RETRY_AT
                .lock()
                .ok()
                .and_then(|state| *state)
                .is_some_and(|time| time > Instant::now())
            {
                return Err(failure(
                    WebErrorCategory::RateLimited,
                    "Brave's Retry-After window is still active. Wait before searching again.",
                ));
            }
            let mut url = Url::parse(BRAVE).map_err(|_| {
                failure(
                    WebErrorCategory::SearchUnavailable,
                    "Search could not start.",
                )
            })?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("count", "10");
            let client = pinned_client(&url).await?;
            self.request(&url, client).await
        })
    }
}
fn provider_delay(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let numbers = |name: &str| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .filter(|s| s.len() <= 1024)
            .map(|s| {
                s.split(',')
                    .take(16)
                    .map(|s| s.trim().parse::<u64>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    numbers("x-ratelimit-remaining")
        .into_iter()
        .zip(numbers("x-ratelimit-reset"))
        .filter_map(|(remaining, reset)| (remaining == Some(0)).then_some(reset).flatten())
        .max()
}
fn retry_delay(headers: &reqwest::header::HeaderMap) -> u64 {
    let value = headers.get("retry-after").and_then(|h| h.to_str().ok());
    let retry = value.and_then(|s| {
        s.parse::<u64>().ok().or_else(|| {
            httpdate::parse_http_date(s).ok().map(|time| {
                time.duration_since(std::time::SystemTime::now())
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_add(1)
            })
        })
    });
    retry
        .into_iter()
        .chain(provider_delay(headers))
        .max()
        .unwrap_or(60)
        .max(1)
}
impl BraveSearch {
    async fn request(
        &self,
        url: &Url,
        client: reqwest::Client,
    ) -> Result<Vec<SearchResult>, WebError> {
        let mut token = reqwest::header::HeaderValue::from_str(&self.key).map_err(|_| {
            failure(
                WebErrorCategory::SearchAuthentication,
                "Brave's key is invalid. Replace it in Settings → Web access.",
            )
        })?;
        token.set_sensitive(true);
        let response = client
            .get(url.clone())
            .header("Accept", "application/json")
            .header("X-Subscription-Token", token)
            .send()
            .await
            .map_err(|error| error::transport_error(&error, url))?;
        let status = response.status();
        let delay = retry_delay(response.headers());
        if status == StatusCode::TOO_MANY_REQUESTS || provider_delay(response.headers()).is_some() {
            let wait = if status == StatusCode::TOO_MANY_REQUESTS {
                delay
            } else {
                provider_delay(response.headers()).unwrap_or(1).max(1)
            };
            if let Ok(mut time) = RETRY_AT.lock() {
                *time = Some(
                    Instant::now()
                        .checked_add(Duration::from_secs(wait))
                        .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 86400)),
                );
            }
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| error::transport_error(&e, url))?;
            if bytes.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
                return Err(failure(
                    WebErrorCategory::SizeLimit,
                    "Brave's response exceeded the 2 MiB limit.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let result = parse_response(status, &bytes);
        if status == StatusCode::TOO_MANY_REQUESTS {
            return result.map_err(|mut e| {
                e.message.push_str(&format!(
                    " Wait at least {delay} seconds. No automatic retry was made."
                ));
                e
            });
        }
        result
    }
}
fn parse_response(status: StatusCode, bytes: &[u8]) -> Result<Vec<SearchResult>, WebError> {
    if !status.is_success() {
        let body = serde_json::from_slice::<Value>(bytes).unwrap_or_default();
        let code = body["error"]["code"].as_str().unwrap_or("");
        let quota = matches!(code, "QUOTA_LIMITED" | "USAGE_LIMIT_EXCEEDED");
        let authentication = matches!(
            code,
            "SUBSCRIPTION_TOKEN_INVALID"
                | "SUBSCRIPTION_TOKEN_NOT_FOUND"
                | "SUBSCRIPTION_NOT_FOUND"
        );
        let (category, message) = if quota || status == StatusCode::PAYMENT_REQUIRED {
            (WebErrorCategory::SearchQuota, "Brave's search quota is exhausted. Review your provider plan or select another provider in Settings → Web access.")
        } else if authentication
            || matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        {
            (WebErrorCategory::SearchAuthentication, "Brave rejected the access key. Replace it in Settings → Web access and check your active provider plan.")
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            (WebErrorCategory::RateLimited, "Brave rate-limited search.")
        } else {
            (WebErrorCategory::SearchUnavailable, "Brave search is unavailable. Check provider status or choose another provider in Settings → Web access. Redirects are refused; your key was not forwarded.")
        };
        return Err(failure(category, message));
    }
    let body: Value = serde_json::from_slice(bytes).map_err(|_| {
        failure(
            WebErrorCategory::SearchUnavailable,
            "Brave returned invalid search data.",
        )
    })?;
    let Some(results) = body["web"]["results"].as_array() else {
        return Err(failure(
            WebErrorCategory::SearchUnavailable,
            "Brave returned no web results. Try a more specific query.",
        ));
    };
    let results: Vec<_> = results
        .iter()
        .take(10)
        .filter_map(|item| {
            let url = checked_url(item["url"].as_str()?).ok()?;
            validate_literal_address(&url).ok()?;
            let clean = |s: &str, cap| {
                scraper::Html::parse_fragment(s)
                    .root_element()
                    .text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(cap)
                    .collect::<String>()
            };
            Some(SearchResult {
                title: clean(item["title"].as_str().unwrap_or(""), 300),
                url: url.to_string(),
                snippet: clean(item["description"].as_str().unwrap_or(""), 1500),
            })
        })
        .collect();
    if results.is_empty() {
        return Err(failure(
            WebErrorCategory::SearchUnavailable,
            "Brave returned no usable public results. Try another query.",
        ));
    }
    Ok(results)
}
#[allow(clippy::too_many_arguments)]
async fn duckduckgo(
    url: &str,
    _query: &str,
    purpose: &str,
    request_id: &str,
    approvals: &Approvals,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
    _runtime: Option<&WebRuntime>,
) -> Result<String, ToolError> {
    #[cfg(test)]
    if let Some(provider) = _runtime.and_then(|r| r.test_duckduckgo.as_ref()) {
        return format_results(url, provider.search(_query).await?);
    }
    retrieve(url, purpose, request_id, approvals, emit, true, |url| {
        Box::pin(pinned_client(url))
    })
    .await
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    query: &str,
    purpose: &str,
    request_id: &str,
    approvals: &Approvals,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
    runtime: Option<&WebRuntime>,
    settings: &WebSettings,
) -> Result<String, ToolError> {
    let url = search_url(query)?; // Validate before approvals, credentials or network.
    if settings.provider == Provider::Duckduckgo {
        if !runtime.is_some_and(|r| r.challenged(request_id)) {
            let result =
                duckduckgo(&url, query, purpose, request_id, approvals, emit, runtime).await;
            match result {
                Err(ToolError::Web(ref e)) if e.category == WebErrorCategory::Challenge => {
                    if let Some(runtime) = runtime {
                        runtime.block_challenge(request_id);
                    }
                    if !settings.brave_fallback {
                        return result;
                    }
                }
                other => return other,
            }
        } else if !settings.brave_fallback {
            return Err(failure(WebErrorCategory::Challenge, "DuckDuckGo challenged this run. It will not be retried. Open Settings → Web access to select Brave or enable its fallback; alternatively use a known public URL.").into());
        }
    }
    if !approvals.request_web(request_id, "api.search.brave.com", format!("Search with Brave Web Search API\nSearch query: {query}\nPurpose: {purpose}\n\nBrave receives this query and your network address. Your provider plan may charge for this request. Results are untrusted.{}", if settings.provider == Provider::Duckduckgo { " The owner enabled Brave as fallback after a DuckDuckGo challenge." } else { "" }), emit).await.map_err(ToolError::Execution)? { return Err(ToolError::Denied); }
    let runtime = runtime.ok_or_else(|| {
        failure(
            WebErrorCategory::SearchAuthentication,
            "Configure Brave in Settings → Web access.",
        )
    })?;
    let results = runtime.search_brave(query).await?;
    format_results(BRAVE, results)
}
pub(super) async fn configured_brave(
    credentials: &dyn SearchCredentials,
    query: &str,
) -> Result<Vec<SearchResult>, WebError> {
    let key = credentials.brave_key().await.map_err(|_| failure(WebErrorCategory::SearchAuthentication, "Brave's key could not be read from the platform credential store. Unlock it and check Settings → Web access."))?
        .filter(|key| !key.trim().is_empty()).ok_or_else(|| failure(WebErrorCategory::SearchAuthentication, "Brave's access key is missing. Save one in Settings → Web access, or select DuckDuckGo."))?;
    BraveSearch::new(key).search(query).await
}
pub(super) fn format_results(
    source: &str,
    mut results: Vec<SearchResult>,
) -> Result<String, ToolError> {
    results.truncate(10);
    loop {
        let body = serde_json::to_string(&results).map_err(|_| ToolError::Arguments)?;
        if body.chars().count() <= MAX_TEXT_CHARS || results.is_empty() {
            return Ok(format!("Source: {source}\nUntrusted search results (fetching a result requires public-network and host validation):\n{body}"));
        }
        results.pop();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn api_results_and_failures_are_bounded_and_sanitized() {
        let results = parse_response(StatusCode::OK, br#"{"web":{"results":[{"title":"<b>Useful</b>","url":"https://example.com/","description":"A snippet"},{"url":"http://127.0.0.1/"},{"url":"https://secret:password@example.com/"}]}}"#).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Useful");
        for (status, expected) in [
            (401, WebErrorCategory::SearchAuthentication),
            (403, WebErrorCategory::SearchAuthentication),
            (402, WebErrorCategory::SearchQuota),
            (429, WebErrorCategory::RateLimited),
            (302, WebErrorCategory::SearchUnavailable),
        ] {
            let error = parse_response(
                StatusCode::from_u16(status).unwrap(),
                b"sensitive-provider-error",
            )
            .unwrap_err();
            assert_eq!(error.category, expected);
            assert!(!error.to_string().contains("sensitive-provider-error"));
        }
        assert_eq!(
            parse_response(
                StatusCode::TOO_MANY_REQUESTS,
                br#"{"error":{"code":"QUOTA_LIMITED"}}"#
            )
            .unwrap_err()
            .category,
            WebErrorCategory::SearchQuota
        );
    }
}

#[cfg(test)]
mod http_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use axum::{
        http::{HeaderMap, StatusCode as HttpStatus},
        routing::get,
        Router,
    };
    use std::{future::IntoFuture, sync::Arc};
    #[tokio::test]
    async fn api_transport_sends_key_only_to_one_origin_and_never_follows_redirects() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(axum::serve(listener, Router::new().route("/{*path}", get(move |uri: axum::http::Uri, headers: HeaderMap| {
            let seen = observed.clone(); async move {
                seen.lock().unwrap().push((uri.path().to_owned(), headers["x-subscription-token"].to_str().unwrap().to_owned()));
                if uri.path() == "/redirect" { (HttpStatus::FOUND, [("location", "http://unapproved.test/key-leak")], "".to_owned()) }
                else { (HttpStatus::OK, [("content-type", "application/json")], r#"{"web":{"results":[{"title":"API fixture","url":"https://example.com/","description":"useful snippet"}]}}"#.to_owned()) }
            }
        }))).into_future());
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve("provider.test", address)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let provider = BraveSearch::new("fixture-key".into());
        let results = provider
            .request(
                &Url::parse("http://provider.test/search?q=private-query").unwrap(),
                client.clone(),
            )
            .await
            .unwrap();
        assert_eq!(results[0].snippet, "useful snippet");
        let error = provider
            .request(
                &Url::parse("http://provider.test/redirect").unwrap(),
                client,
            )
            .await
            .unwrap_err();
        assert_eq!(error.category, WebErrorCategory::SearchUnavailable);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("/search".into(), "fixture-key".into()),
                ("/redirect".into(), "fixture-key".into())
            ]
        );
        assert!(
            !error.to_string().contains("fixture-key")
                && !error.to_string().contains("private-query")
        );
        task.abort();
    }
    #[test]
    fn retry_after_dates_and_seconds_and_large_result_output_are_bounded() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "120".parse().unwrap());
        assert_eq!(retry_delay(&headers), 120);
        let date =
            httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(3600));
        headers.insert("retry-after", date.parse().unwrap());
        assert!((3598..=3601).contains(&retry_delay(&headers)));
        headers.insert("x-ratelimit-remaining", "1, 0".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1, 1419704".parse().unwrap());
        assert_eq!(retry_delay(&headers), 1419704);
        headers.remove("x-ratelimit-remaining");
        headers.remove("x-ratelimit-reset");
        assert_eq!(
            parse_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                br#"{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID"}}"#
            )
            .unwrap_err()
            .category,
            WebErrorCategory::SearchAuthentication
        );
        headers.insert("retry-after", "untrusted header text".parse().unwrap());
        assert_eq!(retry_delay(&headers), 60);
        let results = (0..10)
            .map(|_| SearchResult {
                title: "a".repeat(300),
                url: format!("https://example.com/{}", "b".repeat(4000)),
                snippet: "c".repeat(1500),
            })
            .collect();
        let output = format_results(BRAVE, results).unwrap();
        assert!(output.len() < MAX_TEXT_CHARS + 300);
    }
}
