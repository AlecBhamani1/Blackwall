use super::{
    error::source_url, WebError, WebErrorCategory as Category, MAX_DOWNLOAD_BYTES, MAX_TEXT_CHARS,
};
use ego_tree::iter::Edge;
use futures_util::StreamExt;
use reqwest::{header::HeaderMap, StatusCode, Url};
use scraper::{Html, Node, Selector};

pub(super) async fn read_response(
    response: reqwest::Response,
    url: &Url,
    search: bool,
) -> Result<String, WebError> {
    let status = response.status();
    let headers = response.headers().clone();
    if header(&headers, "cf-mitigated").eq_ignore_ascii_case("challenge") {
        return Err(challenge(url, search));
    }
    if !status.is_success() {
        let (category, message) = match status {
            StatusCode::UNAUTHORIZED => (Category::Authentication, "The website requires login (HTTP 401). Use a public source; web tools do not share browser logins."),
            StatusCode::FORBIDDEN => (Category::SiteDenied, "The website refused this request (HTTP 403). Approval cannot override a site's restrictions. Use another source."),
            StatusCode::TOO_MANY_REQUESTS => (Category::RateLimited, "The website rate-limited this request (HTTP 429). Wait before trying again or use another source."),
            _ => (Category::Http, "The website returned an unsuccessful HTTP response. Try another source."),
        };
        return Err(WebError::new(
            category,
            url,
            &format!("{message} Status: {}.", status.as_u16()),
        ));
    }
    let content_type = header(&headers, "content-type");
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !(mime.starts_with("text/")
        || matches!(mime.as_str(), "application/json" | "application/xhtml+xml"))
    {
        return Err(WebError::new(Category::UnsupportedDocument, url,
            "This is not a supported text document. Use an HTML, plain-text or JSON source; PDF, image and binary downloads are unsupported."));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOWNLOAD_BYTES as u64)
    {
        return Err(size_error(url));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| super::error::transport_error(&error, url))?;
        // reqwest yields decompressed bytes. Enforce the cap while streaming, including gzip bombs.
        if chunk.len() > MAX_DOWNLOAD_BYTES.saturating_sub(bytes.len()) {
            return Err(size_error(url));
        }
        bytes.extend_from_slice(&chunk);
    }
    render(&bytes, content_type, url, search)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

fn size_error(url: &Url) -> WebError {
    WebError::new(Category::SizeLimit, url,
        "The page exceeded the 2 MiB decompressed download limit. Use a smaller page or another source.")
}

fn challenge(url: &Url, search: bool) -> WebError {
    WebError::new(
        Category::Challenge,
        url,
        if search {
            "The search provider returned a human-verification challenge, not search results. Search is unavailable for this request. Select Brave or enable its fallback in Settings → Web access, or use a known public URL with web_fetch; do not retry the challenge automatically."
        } else {
            "The website requires human verification or a browser challenge. Web reads cannot complete it. Use another public source; approval cannot bypass this restriction."
        },
    )
}

fn decode(bytes: &[u8], content_type: &str, url: &Url) -> Result<String, WebError> {
    let charset = content_type.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        name.eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['\'', '"']))
    });
    let meta_charset = if charset.is_none() && content_type.to_ascii_lowercase().contains("html") {
        let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        let head = Html::parse_document(&prefix);
        select(&head, "meta[charset]")
            .and_then(|meta| meta.value().attr("charset").map(str::to_owned))
    } else {
        None
    };
    let encoding = if let Some((encoding, _)) = encoding_rs::Encoding::for_bom(bytes) {
        Some(encoding)
    } else if let Some(label) = charset.or(meta_charset.as_deref()) {
        encoding_rs::Encoding::for_label(label.as_bytes())
    } else {
        Some(encoding_rs::UTF_8)
    }
    .ok_or_else(|| {
        WebError::new(
            Category::Encoding,
            url,
            "The page declares an unsupported text encoding. Use another source.",
        )
    })?;
    let (text, _, errors) = encoding.decode(bytes);
    if errors {
        return Err(WebError::new(
            Category::Encoding,
            url,
            "The page contains invalid text for its declared encoding. Use another source.",
        ));
    }
    Ok(text.into_owned())
}

pub(super) fn render(
    bytes: &[u8],
    content_type: &str,
    url: &Url,
    search: bool,
) -> Result<String, WebError> {
    let text = decode(bytes, content_type, url)?;
    let html = search
        || content_type.to_ascii_lowercase().contains("html")
        || text
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("<!doctype html")
        || text.trim_start().to_ascii_lowercase().starts_with("<html");
    let (title, text) = if html {
        extract_html(&text, url, search)?
    } else {
        (String::new(), text)
    };
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(MAX_TEXT_CHARS).collect();
    let truncated = chars.next().is_some();
    Ok(format!(
        "Source: {}\nTitle: {}\nUntrusted page content{}:\n{}",
        source_url(url),
        title,
        if truncated {
            " (trimmed to 32,000 characters)"
        } else {
            ""
        },
        clipped
    ))
}

fn select<'a>(document: &'a Html, selector: &str) -> Option<scraper::ElementRef<'a>> {
    Selector::parse(selector)
        .ok()
        .and_then(|selector| document.select(&selector).next())
}

fn extract_html(text: &str, url: &Url, search: bool) -> Result<(String, String), WebError> {
    let document = Html::parse_document(text);
    let title = select(&document, "title")
        .map(|title| title.text().collect::<String>())
        .unwrap_or_default();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower_title = title.to_ascii_lowercase();
    let title: String = title.chars().take(300).collect();
    // Prefer structural markers and interstitial titles, not words in an article about challenges.
    if select(&document, "form#challenge-form, form[action*='anomaly.js'], #cf-challenge-running, #challenge-running").is_some()
        || matches!(lower_title.as_str(), "just a moment..." | "attention required! | cloudflare" | "verify you are human" | "checking your browser")
    {
        return Err(challenge(url, search));
    }
    if search {
        return extract_search(&document, url).map(|text| (title, text));
    }
    let root = select(&document, "article")
        .or_else(|| select(&document, "main"))
        .or_else(|| select(&document, "body"))
        .unwrap_or_else(|| document.root_element());
    let mut output = String::new();
    let mut hidden_depth: usize = 0;
    // A single iterative traversal avoids recursion and repeated ancestor walks on hostile HTML.
    for edge in root.traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Element(element) => {
                    if hidden_depth > 0
                        || matches!(
                            element.name(),
                            "script"
                                | "style"
                                | "noscript"
                                | "template"
                                | "nav"
                                | "footer"
                                | "header"
                                | "svg"
                        )
                        || element.attr("hidden").is_some()
                        || element.attr("aria-hidden") == Some("true")
                    {
                        hidden_depth += 1;
                    } else if matches!(
                        element.name(),
                        "p" | "div" | "br" | "li" | "h1" | "h2" | "h3" | "section" | "tr"
                    ) {
                        output.push('\n');
                    }
                }
                Node::Text(text) if hidden_depth == 0 => {
                    output.push_str(text);
                    output.push(' ');
                }
                _ => {}
            },
            Edge::Close(node) if node.value().is_element() => {
                hidden_depth = hidden_depth.saturating_sub(1);
                if hidden_depth == 0 {
                    output.push('\n');
                }
            }
            _ => {}
        }
    }
    let output = output
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if output.trim().is_empty() {
        return Err(WebError::new(Category::RenderingRequired, url,
            "The page has no readable static text. It may need JavaScript or login. Enable optional rendering in Settings → Web access, then use Render page; alternatively use a public static source."));
    }
    Ok((title, output))
}

fn extract_search(document: &Html, url: &Url) -> Result<String, WebError> {
    let selector = Selector::parse("a.result__a").map_err(|_| {
        WebError::new(
            Category::SearchUnavailable,
            url,
            "Search results could not be parsed.",
        )
    })?;
    let mut results = Vec::new();
    for link in document.select(&selector).take(10) {
        let Some(href) = link
            .value()
            .attr("href")
            .and_then(|href| url.join(href).ok())
        else {
            continue;
        };
        let target = if href
            .host_str()
            .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
        {
            href.query_pairs()
                .find(|(key, _)| key == "uddg")
                .and_then(|(_, value)| Url::parse(&value).ok())
        } else {
            Some(href)
        };
        let Some(target) = target.filter(|target| {
            super::checked_url(target.as_str()).is_ok()
                && super::validate_literal_address(target).is_ok()
        }) else {
            continue;
        };
        let title = link
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let snippet = link
            .ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .find(|element| element.value().classes().any(|class| class == "result"))
            .and_then(|element| {
                Selector::parse(".result__snippet")
                    .ok()
                    .and_then(|selector| {
                        element
                            .select(&selector)
                            .next()
                            .map(|snippet| snippet.text().collect::<String>())
                    })
            })
            .unwrap_or_default();
        results.push(super::SearchResult {
            title: title.chars().take(300).collect(),
            url: source_url(&target),
            snippet: snippet.chars().take(1500).collect(),
        });
    }
    if results.is_empty() {
        return Err(WebError::new(Category::SearchUnavailable, url,
            "The search provider returned no usable result links. It may have no matches or its response format may have changed. Use a known public URL or an owner-configured search MCP server."));
    }
    serde_json::to_string(&results).map_err(|_| {
        WebError::new(
            Category::SearchUnavailable,
            url,
            "Search results could not be encoded.",
        )
    })
}
