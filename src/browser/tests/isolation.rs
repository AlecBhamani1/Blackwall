#![allow(clippy::unwrap_used)]
use base64::{engine::general_purpose::STANDARD, Engine};
use blackwall_browser::PlaywrightRenderer;
use blackwall_core::{
    tools::ToolError,
    web::{PageRenderer, RenderNetwork, Resource},
};
use futures_util::future::BoxFuture;
use std::{path::PathBuf, sync::Mutex, time::Duration};

struct Fixture {
    requests: Mutex<Vec<String>>,
    html: &'static str,
}
impl RenderNetwork for Fixture {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Resource, ToolError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(url.into());
            let (content_type, body) = match url {
                "https://public.test/redirect" => {
                    return Ok(Resource {
                        status: 302,
                        content_type: "text/html".into(),
                        body: String::new(),
                        location: Some("https://public.test/app".into()),
                    })
                }
                "https://public.test/app" => ("text/html", self.html),
                "https://assets.test/app.js" => (
                    "application/javascript",
                    r#"
                    fetch('https://api.test/table').then(r=>r.json()).then(rows=>{
                      document.querySelector('#root').innerHTML='<table><tr><td>'+rows.name+'</td><td>'+rows.score+'</td></tr></table>';
                    });
                    try { new Worker('https://private.test/worker.js'); } catch {}
                    try { new SharedWorker('https://private.test/shared.js'); } catch {}
                    new WebSocket('wss://private.test/ws');
                    fetch('https://private.test/upload', {method:'POST',body:'do not send'}).catch(()=>{});
                "#,
                ),
                "https://api.test/table" => (
                    "application/json",
                    r#"{"name":"Deterministic benchmark","score":"98.1"}"#,
                ),
                "https://frame.test/page" => ("text/html", "<p>Isolated frame</p>"),
                _ => return Err(ToolError::Denied),
            };
            Ok(Resource {
                status: 200,
                content_type: content_type.into(),
                body: STANDARD.encode(body),
                location: None,
            })
        })
    }
}
fn renderer() -> PlaywrightRenderer {
    PlaywrightRenderer::new(PathBuf::from(
        std::env::var("BLACKWALL_BROWSER_TEST_DIR").unwrap(),
    ))
}
#[tokio::test]
#[ignore = "offline Chromium integration; install explicitly and set BLACKWALL_BROWSER_TEST_DIR"]
async fn javascript_tables_cross_origin_resources_and_worker_websocket_upload_denials() {
    let network = Fixture { requests: Mutex::default(), html: "<title>Fixture</title><main id='root'></main><div style='display:none'>hidden fixture</div><iframe src='https://frame.test/page'></iframe><script src='https://assets.test/app.js'></script>" };
    let page = renderer()
        .render("https://public.test/redirect", &network)
        .await
        .unwrap();
    assert!(page.html.contains("Deterministic benchmark") && page.html.contains("98.1"));
    assert!(page.html.contains("hidden=\"\""));
    let requests = network.requests.lock().unwrap();
    assert!(requests.iter().any(|u| u == "https://assets.test/app.js"));
    assert!(requests.iter().any(|u| u == "https://api.test/table"));
    assert!(requests.iter().any(|u| u == "https://frame.test/page"));
    assert!(
        !requests.iter().any(|u| u.contains("private.test")),
        "{requests:?}"
    );
}
#[tokio::test]
#[ignore = "offline Chromium integration; install explicitly and set BLACKWALL_BROWSER_TEST_DIR"]
async fn fresh_contexts_do_not_retain_storage_and_missing_binaries_are_actionable() {
    let first = Fixture { requests: Mutex::default(), html: "<script>localStorage.setItem('owner','private');document.cookie='owner=private'</script><p>First</p>" };
    renderer()
        .render("https://public.test/app", &first)
        .await
        .unwrap();
    let second = Fixture { requests: Mutex::default(), html: "<body><script>document.body.textContent = localStorage.getItem('owner') || document.cookie || 'clean context'</script>" };
    let page = renderer()
        .render("https://public.test/app", &second)
        .await
        .unwrap();
    assert!(page.html.contains("clean context"));
    assert!(!page.html.contains("private"));
    assert_eq!(
        PlaywrightRenderer::new(PathBuf::from("/nonexistent-blackwall-browser"))
            .render("https://public.test/app", &second)
            .await
            .unwrap_err()
            .category,
        blackwall_core::web::WebErrorCategory::RendererUnavailable
    );
}
struct Hanging;
impl RenderNetwork for Hanging {
    fn get<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Resource, ToolError>> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test]
#[ignore = "offline Chromium integration; install explicitly and set BLACKWALL_BROWSER_TEST_DIR"]
async fn cancellation_releases_processes_and_render_concurrency() {
    let browser = renderer();
    assert!(tokio::time::timeout(
        Duration::from_millis(1500),
        browser.render("https://public.test/app", &Hanging)
    )
    .await
    .is_err());
    let fixture = Fixture {
        requests: Mutex::default(),
        html: "<p>After cancellation</p>",
    };
    let page = browser
        .render("https://public.test/app", &fixture)
        .await
        .unwrap();
    assert!(page.html.contains("After cancellation"));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn os_network_denial_covers_unintercepted_child_connections() {
    use std::process::Stdio;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let status = tokio::process::Command::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            "(version 1)(allow default)(deny network*)",
            "/bin/sh",
            "-c",
            "exec /usr/bin/curl --noproxy '*' --max-time 1 --silent \"$1\"",
            "fixture",
            &url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(!status.success());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

struct Redirects;
impl RenderNetwork for Redirects {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Resource, ToolError>> {
        Box::pin(async move {
            let location = if url.ends_with("/loop") {
                url.to_owned()
            } else {
                let index: usize = url.rsplit('/').next().unwrap().parse().unwrap();
                format!("https://public.test/hop/{}", index + 1)
            };
            Ok(Resource {
                status: 302,
                content_type: "text/html".into(),
                body: String::new(),
                location: Some(location),
            })
        })
    }
}
#[tokio::test]
#[ignore = "offline Chromium integration; install explicitly and set BLACKWALL_BROWSER_TEST_DIR"]
async fn redirects_dom_requests_and_waits_have_typed_bounds() {
    use blackwall_core::web::WebErrorCategory as Category;
    for (url, category) in [
        ("https://public.test/loop", Category::RedirectLoop),
        ("https://public.test/hop/0", Category::RedirectLimit),
    ] {
        let error = renderer().render(url, &Redirects).await.unwrap_err();
        assert_eq!(error.category, category, "{error}");
    }
    for (html, category) in [
        ("<body><script>document.body.append('x'.repeat(3*1024*1024))</script>", Category::SizeLimit),
        ("<body><script>for(let i=0;i<150;i++){const image=new Image();image.src='https://image.test/'+i;document.body.append(image)}</script>", Category::SizeLimit),
        ("<script>while(true){}</script>", Category::Timeout),
    ] {
        let fixture = Fixture { requests: Mutex::default(), html };
        let error = renderer().render("https://public.test/app", &fixture).await.unwrap_err();
        assert_eq!(error.category, category, "{error}");
    }
}
