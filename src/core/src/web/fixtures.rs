#![allow(clippy::unwrap_used)]
use super::*;
use crate::protocol::{ApprovalDecision, ResolveApprovalRequest};
use axum::{body::Body, http::Response, routing::get, Router};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

async fn fixture(uri: axum::http::Uri) -> Response<Body> {
    let path = uri.path();
    let mut status = 200;
    let mut mime = "text/html; charset=utf-8";
    let mut location = None;
    let mut encoding = None;
    let body = match path {
        "/plain" => { mime = "text/plain"; b"A public text source".to_vec() }
        "/article" => b"<title>Source &amp; title</title><nav>Navigation</nav><main><h1>Heading</h1><p>Readable &amp; useful text.</p><script>secret script</script><style>secret style</style><div hidden>hidden text</div><p>After hidden</p></main>".to_vec(),
        "/large" => format!("<title>Large</title><script>{}</script><article>Useful article after 300 KiB of scripts.</article>", "x".repeat(300 * 1024)).into_bytes(),
        "/oversize" => vec![b'x'; MAX_DOWNLOAD_BYTES + 1],
        "/chunked" => {
            let chunks = futures_util::stream::iter((0..33).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 64 * 1024])));
            return Response::builder().header("content-type", "text/plain").body(Body::from_stream(chunks)).unwrap();
        }
        "/gzip" => { encoding = Some("gzip"); gzip(b"<title>Compressed</title><p>Readable compressed page</p>") }
        "/bomb" => { encoding = Some("gzip"); gzip(&vec![b'x'; MAX_DOWNLOAD_BYTES + 1]) }
        "/bad-gzip" => { encoding = Some("gzip"); b"not compressed".to_vec() }
        "/403" => { status = 403; b"Forbidden".to_vec() }
        "/401" => { status = 401; b"Login".to_vec() }
        "/429" => { status = 429; b"Slow down".to_vec() }
        "/challenge" | "/challenge202" => {
            if path.ends_with("202") { status = 202; }
            b"<title>DuckDuckGo</title><form id='challenge-form' action='/anomaly.js'>Select the ducks</form>".to_vec()
        }
        "/cf" => b"<title>Just a moment...</title><script src='/cdn-cgi/challenge-platform/foo'></script>".to_vec(),
        "/cf-header" => return Response::builder().status(403).header("cf-mitigated", "challenge").body(Body::empty()).unwrap(),
        "/results" => b"<title>Search</title><a class='result__a' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Farticle'>A useful result</a><a class='result__a' href='http://127.0.0.1/'>Private</a>".to_vec(),
        "/no-results" => b"<title>Search</title><p>No results</p>".to_vec(),
        "/js" => b"<title>App</title><div id='root'></div><script>render()</script><noscript>Enable JS</noscript>".to_vec(),
        "/pdf" => { mime = "application/pdf"; b"%PDF".to_vec() }
        "/latin" => { mime = "TEXT/PLAIN; Charset=windows-1252"; b"caf\xe9".to_vec() }
        "/json" => { mime = "application/json"; br#"{"answer":42}"#.to_vec() }
        "/invalid-text" => { mime = "text/plain"; vec![0xff] }
        "/unknown-charset" => { mime = "text/plain; charset=unknown"; b"text".to_vec() }
        "/loop" => { status = 302; location = Some("/loop#fragment".into()); vec![] }
        "/private" => { status = 302; location = Some("http://127.0.0.1/secret".into()); vec![] }
        "/credentials" => { status = 302; location = Some("https://owner:secret@example.com/".into()); vec![] }
        "/other-host" => { status = 302; location = Some("http://other.test/plain".into()); vec![] }
        "/slow" => { tokio::time::sleep(Duration::from_millis(100)).await; b"slow".to_vec() }
        _ if path.starts_with("/redirect/") => {
            let index = path.trim_start_matches("/redirect/").parse::<usize>().unwrap();
            if index < 4 { status = 302; location = Some(format!("/redirect/{}", index + 1)); }
            b"<title>Final</title><p>Final readable page</p>".to_vec()
        }
        _ if path.starts_with("/forever/") => {
            status = 302;
            let index = path.trim_start_matches("/forever/").parse::<usize>().unwrap();
            location = Some(format!("/forever/{}", index + 1));
            vec![]
        }
        _ => b"<p>Fixture</p>".to_vec(),
    };
    let mut response = Response::builder()
        .status(status)
        .header("content-type", mime);
    if let Some(location) = location {
        response = response.header("location", location);
    }
    if let Some(encoding) = encoding {
        response = response.header("content-encoding", encoding);
    }
    response.body(Body::from(body)).unwrap()
}

struct Server {
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/{*path}", get(fixture)))
                .await
                .unwrap();
        });
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .resolve("fixture.test", address)
                .resolve("other.test", address)
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            task,
        }
    }
    async fn fetch(&self, path: &str, search: bool) -> Result<String, ToolError> {
        let approvals = Approvals::default();
        retrieve(
            &format!("http://fixture.test{path}"),
            "test",
            "fixture",
            &approvals,
            &|event| allow(&approvals, event),
            search,
            |_| {
                let client = self.client.clone();
                Box::pin(async move { Ok(client) })
            },
        )
        .await
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

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

fn category(error: ToolError) -> WebErrorCategory {
    match error {
        ToolError::Web(error) => error.category,
        other => panic!("Unexpected error: {other}"),
    }
}

#[tokio::test]
async fn readable_sources_large_pages_encoding_and_redirects() {
    let server = Server::new().await;
    for (path, expected) in [
        ("/plain", "A public text source"),
        ("/article", "Readable & useful text."),
        ("/large", "Useful article after 300 KiB"),
        ("/gzip", "Readable compressed page"),
        ("/latin", "café"),
        ("/json", "{\"answer\":42}"),
        ("/redirect/0", "Final readable page"),
    ] {
        let result = server.fetch(path, false).await.unwrap();
        assert!(result.contains(expected), "{path}: {result}");
        assert!(
            !result.contains("secret script")
                && !result.contains("secret style")
                && !result.contains("hidden text")
        );
        if path == "/article" {
            assert!(result.contains("Source & title") && result.contains("After hidden"));
        }
        if path == "/redirect/0" {
            assert!(result.contains("Source: http://fixture.test/redirect/4"));
        }
    }
}

#[tokio::test]
async fn failures_are_specific_and_challenges_never_become_results() {
    let server = Server::new().await;
    for (path, expected) in [
        ("/403", WebErrorCategory::SiteDenied),
        ("/401", WebErrorCategory::Authentication),
        ("/429", WebErrorCategory::RateLimited),
        ("/challenge", WebErrorCategory::Challenge),
        ("/challenge202", WebErrorCategory::Challenge),
        ("/cf", WebErrorCategory::Challenge),
        ("/cf-header", WebErrorCategory::Challenge),
        ("/pdf", WebErrorCategory::UnsupportedDocument),
        ("/invalid-text", WebErrorCategory::Encoding),
        ("/unknown-charset", WebErrorCategory::Encoding),
        ("/oversize", WebErrorCategory::SizeLimit),
        ("/chunked", WebErrorCategory::SizeLimit),
        ("/bomb", WebErrorCategory::SizeLimit),
        ("/bad-gzip", WebErrorCategory::Encoding),
        ("/loop", WebErrorCategory::RedirectLoop),
        ("/forever/0", WebErrorCategory::RedirectLimit),
        ("/private", WebErrorCategory::NetworkPolicy),
        ("/credentials", WebErrorCategory::InvalidUrl),
        ("/js", WebErrorCategory::RenderingRequired),
    ] {
        assert_eq!(
            category(server.fetch(path, false).await.unwrap_err()),
            expected,
            "{path}"
        );
    }
    for path in ["/challenge", "/challenge202", "/cf-header"] {
        assert_eq!(
            category(server.fetch(path, true).await.unwrap_err()),
            WebErrorCategory::Challenge
        );
    }
    assert_eq!(
        category(server.fetch("/no-results", true).await.unwrap_err()),
        WebErrorCategory::SearchUnavailable
    );
    let results = server.fetch("/results", true).await.unwrap();
    assert!(results.contains("A useful result\nhttps://example.com/article"));
    assert!(!results.contains("127.0.0.1"));
}

#[tokio::test]
async fn every_redirect_requires_permission_and_denials_never_send() {
    let server = Server::new().await;
    let approvals = Approvals::default();
    let sources = Arc::new(Mutex::new(vec![]));
    let result = retrieve(
        "http://fixture.test/other-host?token=secret#private",
        "read",
        "denial",
        &approvals,
        &|event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                detail,
                ..
            } = event
            {
                assert!(!detail.contains("secret") && !detail.contains("#private"));
                sources.lock().unwrap().push(detail);
                let decision = if sources.lock().unwrap().len() == 1 {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Deny
                };
                approvals
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision,
                    })
                    .unwrap();
            }
        },
        false,
        |_| {
            let client = server.client.clone();
            Box::pin(async move { Ok(client) })
        },
    )
    .await;
    assert!(matches!(result, Err(ToolError::Denied)));
    assert_eq!(sources.lock().unwrap().len(), 2);
}

#[test]
fn dns_rebinding_mixed_addresses_and_url_credentials_are_rejected() {
    let url = Url::parse("https://example.com/path?token=secret#private").unwrap();
    let public = "1.1.1.1:443".parse().unwrap();
    let private = "127.0.0.1:443".parse().unwrap();
    assert!(validate_addresses(&url, &[public]).is_ok());
    assert_eq!(
        validate_addresses(&url, &[public, private])
            .unwrap_err()
            .category,
        WebErrorCategory::NetworkPolicy
    );
    assert_eq!(
        validate_addresses(&url, &[]).unwrap_err().category,
        WebErrorCategory::Dns
    );
    let error = private_address_error(&url).to_string();
    assert!(!error.contains("secret") && !error.contains("#private"));
    assert!(
        serde_json::from_str::<Value>(&error).unwrap()["webError"]["source"]
            == "https://example.com/path"
    );
    for value in [
        "https://user:secret@example.com/",
        "http://example.com:8080/",
        "file:///etc/passwd",
    ] {
        assert!(checked_url(value).is_err());
    }
}

#[tokio::test]
async fn transport_diagnostics_do_not_echo_query_credentials() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let url = Url::parse(&format!("http://{address}/?tls_key=secret")).unwrap();
    let error = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(url.clone())
        .send()
        .await
        .unwrap_err();
    let failure = error::transport_error(&error, &url);
    assert_eq!(failure.category, WebErrorCategory::Connectivity);
    assert!(!failure.to_string().contains("secret"));
    let server = Server::new().await;
    let url = Url::parse("http://fixture.test/slow?key=secret").unwrap();
    let error = server
        .client
        .get(url.clone())
        .timeout(Duration::from_millis(20))
        .send()
        .await
        .unwrap_err();
    let failure = error::transport_error(&error, &url);
    assert_eq!(failure.category, WebErrorCategory::Timeout);
    assert!(!failure.to_string().contains("secret"));
}

#[test]
fn html_parser_fuzz_is_panic_free_and_bounded() {
    let url = Url::parse("https://example.com/").unwrap();
    let mut seed = 17_u64;
    for length in [0, 1, 16, 256, 4096, 65536] {
        let mut input = String::new();
        for _ in 0..length {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            input.push(char::from(b"<>/ abcdef&;\"'\n"[(seed >> 32) as usize % 15]));
        }
        if let Ok(output) = document::render(input.as_bytes(), "text/html", &url, false) {
            assert!(output.len() <= MAX_TEXT_CHARS * 4 + 1024);
        }
    }
    let nested = format!(
        "{}<p>Readable</p>{}",
        "<div>".repeat(1000),
        "</div>".repeat(1000)
    );
    assert!(
        document::render(nested.as_bytes(), "text/html", &url, false)
            .unwrap()
            .contains("Readable")
    );
    let text = "é".repeat(40_000);
    let output = document::render(text.as_bytes(), "text/plain", &url, false).unwrap();
    assert_eq!(output.matches('é').count(), MAX_TEXT_CHARS);
    assert!(output.contains("trimmed"));
    let search = format!(
        "<title>{}</title><a class='result__a' href='https://example.com/'>Result</a>",
        "é".repeat(40_000)
    );
    assert!(
        document::render(search.as_bytes(), "text/html", &url, true)
            .unwrap()
            .len()
            < 1024
    );
    let ordinary = b"<title>About bot challenges</title><script src='/cdn-cgi/challenge-platform/h/b/scripts/jsd/main.js'></script><article>Ordinary public article</article>";
    assert!(document::render(ordinary, "text/html", &url, false)
        .unwrap()
        .contains("Ordinary public article"));
    assert_eq!(
        document::render(b"A plain response", "text/plain", &url, true)
            .unwrap_err()
            .category,
        WebErrorCategory::SearchUnavailable
    );
    assert!(document::render(
        b"<meta charset='windows-1252'><p>caf\xe9</p>",
        "text/html",
        &url,
        false
    )
    .unwrap()
    .contains("café"));
    assert!(
        document::render(&[0xff, 0xfe, b'A', 0], "text/plain", &url, false)
            .unwrap()
            .ends_with('A')
    );
}

#[tokio::test]
#[ignore = "manual public-network smoke; never runs in the offline suite"]
async fn public_web_smoke() {
    for (url, search) in [
        ("https://example.com/".to_owned(), false),
        (
            "https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CORS".to_owned(),
            false,
        ),
        ("https://github.com/".to_owned(), false),
        (
            "https://en.wikipedia.org/wiki/Rust_(programming_language)".to_owned(),
            false,
        ),
        ("http://example.com/".to_owned(), false),
        (search_url("Rust documentation").unwrap(), true),
    ] {
        let approvals = Approvals::default();
        let result = retrieve(
            &url,
            "Public-site acceptance smoke",
            "smoke",
            &approvals,
            &|event| allow(&approvals, event),
            search,
            |url| Box::pin(pinned_client(url)),
        )
        .await;
        match result {
            Ok(text) => println!(
                "SMOKE {}: OK; {} characters; {}",
                error::source_url(&Url::parse(&url).unwrap()),
                text.chars().count(),
                text.lines().take(2).collect::<Vec<_>>().join("; ")
            ),
            Err(error) => println!(
                "SMOKE {}: {}",
                error::source_url(&Url::parse(&url).unwrap()),
                error
            ),
        }
    }
}
