//! Explicit public-network probe, separate from offline tests. Never starts a model.
use blackwall_browser::PlaywrightRenderer;
use blackwall_core::{
    approvals::Approvals,
    model::{ToolCall, ToolFunction},
    protocol::{AgentEvent, ApprovalDecision, ResolveApprovalRequest},
    web::*,
};
use futures_util::future::BoxFuture;
use std::{path::PathBuf, sync::Arc};
struct NoKey;
impl SearchCredentials for NoKey {
    fn brave_key(&self) -> BoxFuture<'_, Result<Option<String>, String>> {
        Box::pin(async { Ok(None) })
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Supply an absolute disposable data directory")?,
    );
    if !directory.is_absolute() {
        return Err("Use an absolute path".into());
    }
    let browser = Arc::new(PlaywrightRenderer::new(directory.join("browser")));
    if std::env::args().any(|a| a == "--install") {
        browser.install().await?;
    }
    WebSettings {
        rendering: true,
        ..WebSettings::default()
    }
    .save(&directory)
    .await?;
    let approvals = Approvals::default();
    approvals.set_web_runtime(Arc::new(WebRuntime::new(
        directory,
        Arc::new(NoKey),
        Some(browser),
    )))?;
    for url in [
        "https://example.com/",
        "https://crfm.stanford.edu/helm/mmlu/latest/",
    ] {
        let call = ToolCall { id: "probe".into(), function: ToolFunction { name: "web_fetch".into(), arguments: serde_json::json!({"url": url, "purpose": "Explicit owner public-page smoke test"}).to_string() } };
        let output = execute(&call, "probe", &approvals, &|event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                let _ = approvals.resolve(ResolveApprovalRequest {
                    request_id,
                    approval_id,
                    decision: ApprovalDecision::AllowWebForRun,
                });
            }
        })
        .await;
        match output {
            Ok(text) => println!(
                "{url}: {} chars\n{}",
                text.chars().count(),
                text.chars().take(2500).collect::<String>()
            ),
            Err(error) => println!("{url}: {error}"),
        }
    }
    approvals.clear_run("probe");
    Ok(())
}
