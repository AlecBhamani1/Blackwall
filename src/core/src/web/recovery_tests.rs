#![allow(clippy::unwrap_used)]
use super::*;
use crate::{
    model::{ToolCall, ToolFunction},
    protocol::{ApprovalDecision, ResolveApprovalRequest},
};
use futures_util::future::BoxFuture;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub(crate) struct FixtureProvider {
    pub calls: AtomicUsize,
    challenge: bool,
}
impl SearchProvider for FixtureProvider {
    fn search<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Vec<SearchResult>, WebError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.challenge {
                return Err(WebError {
                    category: WebErrorCategory::Challenge,
                    source: "https://html.duckduckgo.com/html/".into(),
                    message: "Challenge fixture".into(),
                });
            }
            Ok(vec![SearchResult {
                title: "Useful fixture".into(),
                url: "https://example.com/".into(),
                snippet: "Normalized API snippet".into(),
            }])
        })
    }
}
struct NoKey;
impl SearchCredentials for NoKey {
    fn brave_key(&self) -> BoxFuture<'_, Result<Option<String>, String>> {
        Box::pin(async { Ok(None) })
    }
}
pub(crate) async fn fixture(
    provider: Provider,
    fallback: bool,
) -> (Arc<WebRuntime>, Arc<FixtureProvider>, Arc<FixtureProvider>) {
    let directory = std::env::temp_dir().join(format!(
        "blackwall-web-recovery-{}",
        crate::pairing::secret("fixture_")
    ));
    WebSettings {
        provider,
        brave_fallback: fallback,
        rendering: true,
    }
    .save(&directory)
    .await
    .unwrap();
    let brave = Arc::new(FixtureProvider {
        calls: AtomicUsize::new(0),
        challenge: false,
    });
    let duck = Arc::new(FixtureProvider {
        calls: AtomicUsize::new(0),
        challenge: true,
    });
    let mut runtime = WebRuntime::new(directory, Arc::new(NoKey), None);
    runtime.test_brave = Some(brave.clone());
    runtime.test_duckduckgo = Some(duck.clone());
    (Arc::new(runtime), brave, duck)
}
fn call() -> ToolCall {
    ToolCall {
        id: "search".into(),
        function: ToolFunction {
            name: "web_search".into(),
            arguments: json!({"query":"owner query", "purpose":"research"}).to_string(),
        },
    }
}
pub(crate) fn allow(approvals: &Approvals, event: AgentEvent) {
    if let AgentEvent::ApprovalRequest {
        request_id,
        approval_id,
        detail,
        ..
    } = event
    {
        assert!(detail.contains("owner query"));
        assert!(detail.contains("charge"));
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
async fn challenge_is_requested_once_and_fallback_requires_owner_selection_and_approval() {
    for fallback in [false, true] {
        let (runtime, brave, duck) = fixture(Provider::Duckduckgo, fallback).await;
        let approvals = Approvals::default();
        approvals.set_web_runtime(runtime.clone()).unwrap();
        for _ in 0..2 {
            let result = execute(&call(), "run", &approvals, &|event| {
                allow(&approvals, event)
            })
            .await;
            assert_eq!(result.is_ok(), fallback);
        }
        assert_eq!(duck.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            brave.calls.load(Ordering::SeqCst),
            if fallback { 2 } else { 0 }
        );
        approvals.clear_run("run");
        assert!(!runtime.challenged("run"));
        tokio::fs::remove_dir_all(&runtime.directory).await.unwrap();
    }
    let (runtime, brave, _) = fixture(Provider::Brave, false).await;
    let approvals = Approvals::default();
    approvals.set_web_runtime(runtime.clone()).unwrap();
    let result = execute(&call(), "denied", &approvals, &|event| {
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
    })
    .await;
    assert!(matches!(result, Err(ToolError::Denied)));
    assert_eq!(brave.calls.load(Ordering::SeqCst), 0);
    tokio::fs::remove_dir_all(&runtime.directory).await.unwrap();
}

struct FixtureRenderer(&'static str);
impl PageRenderer for FixtureRenderer {
    fn render<'a>(
        &'a self,
        url: &'a str,
        _: &'a dyn RenderNetwork,
    ) -> BoxFuture<'a, Result<RenderedPage, WebError>> {
        Box::pin(async move {
            Ok(RenderedPage {
                url: url.into(),
                html: self.0.into(),
            })
        })
    }
}
#[tokio::test]
async fn rendered_pages_keep_extraction_challenge_login_and_output_contracts() {
    for (html, category) in [("<title>App</title><main><table><tr><td>Benchmark</td><td>98.1</td></tr></table></main>", None), ("<title>Just a moment...</title><form id='challenge-form'></form>", Some(WebErrorCategory::Challenge)), ("<title>Login</title><input type='password'>", Some(WebErrorCategory::Authentication))] {
        let (runtime, _, _) = fixture(Provider::Brave, false).await;
        let mut runtime = Arc::try_unwrap(runtime).ok().unwrap(); runtime.renderer = Some(Arc::new(FixtureRenderer(html)));
        let runtime = Arc::new(runtime);
        let approvals = Approvals::default(); approvals.set_web_runtime(runtime.clone()).unwrap();
        let call = ToolCall { id: "render".into(), function: ToolFunction { name: "web_fetch".into(), arguments: json!({"url":"https://example.com/app", "render":true,"purpose":"read"}).to_string() } };
        let result = execute(&call, "render", &approvals, &|event| { if let AgentEvent::ApprovalRequest { request_id, approval_id, .. } = event { approvals.resolve(ResolveApprovalRequest { request_id, approval_id, decision: ApprovalDecision::AllowWebForRun }).unwrap(); } }).await;
        if let Some(category) = category { assert!(matches!(result, Err(ToolError::Web(e)) if e.category == category)); }
        else { let text = result.unwrap(); assert!(text.contains("Benchmark") && text.contains("98.1") && text.contains("Untrusted page content")); }
        WebSettings::default().save(&runtime.directory).await.unwrap();
        assert!(matches!(execute(&call, "disabled", &approvals, &|_| panic!("disabled renderer requested approval")).await, Err(ToolError::Web(e)) if e.category == WebErrorCategory::RendererUnavailable));
        tokio::fs::remove_dir_all(&runtime.directory).await.unwrap();
    }
}
