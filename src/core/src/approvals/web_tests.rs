#![allow(clippy::unwrap_used)]
use super::*;
use crate::policy::{self, RulesFile};
use serde_json::{json, Value};
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("blackwall-web-grant-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }
    fn write(&self, file: Value) {
        std::fs::write(self.0.join(policy::POLICY_FILE), file.to_string()).unwrap();
    }
    async fn approvals(&self) -> Approvals {
        let approvals = Approvals::default();
        approvals.load_global_policy("run", &self.0).await.unwrap();
        approvals
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn answer(approvals: &Approvals, decision: ApprovalDecision) -> impl Fn(AgentEvent) + Send + Sync {
    let approvals = approvals.clone();
    move |event| {
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
                    decision,
                })
                .unwrap();
        }
    }
}
fn never(_: AgentEvent) {
    panic!("permission must avoid a prompt");
}

#[tokio::test]
async fn run_permission_covers_changed_requests_and_hosts_then_expires() {
    let approvals = Approvals::default();
    assert!(approvals
        .request_web(
            "run",
            "duckduckgo.com",
            "query one".into(),
            &answer(&approvals, ApprovalDecision::AllowWebForRun)
        )
        .await
        .unwrap());
    for (host, detail) in [
        ("duckduckgo.com", "query two, different purpose"),
        ("docs.rs", "GET /one"),
        ("docs.rs", "GET /two"),
        ("www.docs.rs", "redirect destination"),
    ] {
        assert!(approvals
            .request_web("run", host, detail.into(), &never)
            .await
            .unwrap());
    }
    let deny = answer(&approvals, ApprovalDecision::Deny);
    assert!(!approvals
        .request_web("other", "docs.rs", "page".into(), &deny)
        .await
        .unwrap());
    approvals.clear_run("run");
    assert!(!approvals
        .request_web("run", "docs.rs", "page".into(), &deny)
        .await
        .unwrap());
}

#[tokio::test]
async fn persistent_permission_survives_restart_and_revocation_applies_to_active_runs() {
    let fixture = Fixture::new();
    fixture.write(json!({"rules":[{"decision":"deny","kind":"file","path":".git"}]}));
    let approvals = fixture.approvals().await;
    assert!(approvals
        .request_web(
            "run",
            "docs.rs",
            "read one".into(),
            &answer(&approvals, ApprovalDecision::AllowWeb)
        )
        .await
        .unwrap());
    assert!(approvals
        .request_web("run", "example.org", "read two".into(), &never)
        .await
        .unwrap());
    let restarted = fixture.approvals().await;
    assert!(restarted
        .request_web("run", "example.net", "search".into(), &never)
        .await
        .unwrap());
    let shown = policy::owner_rules(&fixture.0).unwrap();
    assert_eq!(shown.rules.len(), 2);
    let rules = shown
        .rules
        .iter()
        .filter(|rule| rule.tool.as_deref() != Some(policy::WEB_ACCESS_TOOL))
        .cloned()
        .collect();
    policy::save_owner_rules(&fixture.0, &shown, rules).unwrap();
    for current in [&approvals, &restarted] {
        assert!(!current
            .request_web(
                "run",
                "example.org",
                "after revocation".into(),
                &answer(current, ApprovalDecision::Deny)
            )
            .await
            .unwrap());
    }
    let audit: Vec<Value> = std::fs::read_to_string(fixture.0.join("policy-audit.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(audit
        .iter()
        .any(|entry| entry["permission"] == "saved"
            && entry["saved"]["tool"] == policy::WEB_ACCESS_TOOL));
    assert!(audit
        .iter()
        .any(|entry| entry["permission"] == "policy"
            && entry["capability"] == policy::WEB_ACCESS_TOOL));
    assert_eq!(audit.last().unwrap()["allowed"], false);
    assert_eq!(policy::owner_rules(&fixture.0).unwrap().rules.len(), 1);
}

#[tokio::test]
async fn toggle_grant_does_not_persist_or_authorize_other_capabilities() {
    let fixture = Fixture::new();
    let approvals = fixture.approvals().await;
    approvals.allow_web_for_run("run").unwrap();
    assert!(approvals
        .request_web("run", "docs.rs", "first".into(), &never)
        .await
        .unwrap());
    let deny = answer(&approvals, ApprovalDecision::Deny);
    assert!(!approvals
        .request_http_tool("run", "lookup", "docs.rs", false, "http tool".into(), &deny)
        .await
        .unwrap());
    assert!(!approvals
        .request(
            "run",
            ApprovalKind::Network,
            "docs.rs",
            "generic network".into(),
            &deny
        )
        .await
        .unwrap());
    assert!(!approvals
        .request(
            "run",
            ApprovalKind::Exec,
            "web_fetch",
            "shell".into(),
            &deny
        )
        .await
        .unwrap());
    assert!(!approvals
        .request("run", ApprovalKind::File, "a.txt", "write".into(), &deny)
        .await
        .unwrap());
    let restarted = fixture.approvals().await;
    assert!(!restarted
        .request_web(
            "run",
            "docs.rs",
            "fresh run".into(),
            &answer(&restarted, ApprovalDecision::Deny)
        )
        .await
        .unwrap());
    let audit = std::fs::read_to_string(fixture.0.join("policy-audit.jsonl")).unwrap();
    assert!(audit
        .lines()
        .any(|line| serde_json::from_str::<Value>(line).unwrap()["permission"] == "run"));
}

#[tokio::test]
async fn saved_permission_never_authorizes_owner_http_tools_or_generic_network_requests() {
    let fixture = Fixture::new();
    policy::allow_web(&fixture.0).unwrap();
    let approvals = fixture.approvals().await;
    let deny = answer(&approvals, ApprovalDecision::Deny);
    for tool in ["lookup", policy::WEB_ACCESS_TOOL] {
        assert!(!approvals
            .request_http_tool("run", tool, "docs.rs", false, "http tool".into(), &deny)
            .await
            .unwrap());
    }
    assert!(!approvals
        .request(
            "run",
            ApprovalKind::Network,
            "docs.rs",
            "network".into(),
            &deny
        )
        .await
        .unwrap());
    assert!(!approvals
        .request(
            "run",
            ApprovalKind::Exec,
            policy::WEB_ACCESS_TOOL,
            "command".into(),
            &deny
        )
        .await
        .unwrap());
    assert!(!crate::command_tools::valid_name(policy::WEB_ACCESS_TOOL));
}

#[tokio::test]
async fn explicit_host_rules_override_run_and_saved_permission_even_when_ordered() {
    let fixture = Fixture::new();
    let approvals = fixture.approvals().await;
    approvals
        .request_web(
            "run",
            "docs.rs",
            "first".into(),
            &answer(&approvals, ApprovalDecision::AllowWebForRun),
        )
        .await
        .unwrap();
    fixture.write(json!({"mode":"ordered","rules":[
        {"decision":"allow","kind":"network","tool":policy::WEB_ACCESS_TOOL},
        {"decision":"deny","kind":"network","host":"blocked.example"},
        {"decision":"ask","kind":"network","host":"ask.example"}
    ]}));
    assert!(!approvals
        .request_web("run", "blocked.example", "redirect".into(), &never)
        .await
        .unwrap());
    let prompts = Mutex::new(0);
    for _ in 0..2 {
        assert!(approvals
            .request_web("run", "ask.example", "page".into(), &|event| {
                if let AgentEvent::ApprovalRequest {
                    request_id,
                    approval_id,
                    web_access,
                    ..
                } = event
                {
                    assert_eq!(web_access, None);
                    *prompts.lock().unwrap() += 1;
                    assert!(approvals
                        .resolve(ResolveApprovalRequest {
                            request_id: request_id.clone(),
                            approval_id: approval_id.clone(),
                            decision: ApprovalDecision::AllowWebForRun
                        })
                        .is_err());
                    approvals
                        .resolve(ResolveApprovalRequest {
                            request_id,
                            approval_id,
                            decision: ApprovalDecision::Allow,
                        })
                        .unwrap();
                }
            })
            .await
            .unwrap());
    }
    assert_eq!(*prompts.lock().unwrap(), 2);
}

#[tokio::test]
async fn project_restrictions_remain_the_original_snapshot_when_owner_rules_refresh() {
    let fixture = Fixture::new();
    let workspace = fixture.0.join("project");
    let settings = workspace.join(".blackwall/project.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        json!({"policy":{"rules":[{"decision":"deny","kind":"network","host":"blocked.example"}]}})
            .to_string(),
    )
    .unwrap();
    let approvals = Approvals::default();
    approvals
        .load_policy("run", &fixture.0, &workspace)
        .await
        .unwrap();
    policy::allow_web(&fixture.0).unwrap();
    std::fs::write(&settings, r#"{"policy":{"rules":[]}}"#).unwrap();
    assert!(!approvals
        .request_web("run", "blocked.example", "page".into(), &never)
        .await
        .unwrap());
    assert!(approvals
        .request_web("run", "docs.rs", "page".into(), &never)
        .await
        .unwrap());
}

#[tokio::test]
async fn invalid_policy_disables_reusable_permission_and_failed_save_does_not_run() {
    let fixture = Fixture::new();
    let approvals = fixture.approvals().await;
    let error = approvals
        .request_web("run", "docs.rs", "page".into(), &|event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                web_access,
                ..
            } = event
            {
                assert_eq!(web_access, Some(WebAccessApproval::Persistent));
                std::fs::write(fixture.0.join(policy::POLICY_FILE), "invalid").unwrap();
                approvals
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::AllowWeb,
                    })
                    .unwrap();
            }
        })
        .await
        .unwrap_err();
    assert!(error.contains("not run"));
    assert!(approvals.0.lock().unwrap().web_runs.is_empty());
    let prompted = Mutex::new(false);
    assert!(!approvals
        .request_web("run", "docs.rs", "page".into(), &|event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                web_access,
                detail,
                ..
            } = event
            {
                assert_eq!(web_access, None);
                assert!(detail.contains("not valid"));
                *prompted.lock().unwrap() = true;
                approvals
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Deny,
                    })
                    .unwrap();
            }
        })
        .await
        .unwrap());
    assert!(*prompted.lock().unwrap());
    assert_eq!(
        std::fs::read_to_string(fixture.0.join(policy::POLICY_FILE)).unwrap(),
        "invalid"
    );
}

#[tokio::test]
async fn forged_web_decisions_cannot_expand_an_unrelated_prompt() {
    let approvals = Approvals::default();
    assert!(!approvals
        .request(
            "run",
            ApprovalKind::Exec,
            "ls",
            "command".into(),
            &|event| {
                if let AgentEvent::ApprovalRequest {
                    request_id,
                    approval_id,
                    ..
                } = event
                {
                    for decision in [ApprovalDecision::AllowWeb, ApprovalDecision::AllowWebForRun] {
                        assert!(approvals
                            .resolve(ResolveApprovalRequest {
                                request_id: request_id.clone(),
                                approval_id: approval_id.clone(),
                                decision
                            })
                            .is_err());
                    }
                    approvals
                        .resolve(ResolveApprovalRequest {
                            request_id,
                            approval_id,
                            decision: ApprovalDecision::Deny,
                        })
                        .unwrap();
                }
            }
        )
        .await
        .unwrap());
    assert!(approvals.0.lock().unwrap().web_runs.is_empty());
}

#[tokio::test]
async fn text_web_calls_share_the_resolved_url_gate_without_granting_generic_network() {
    let approvals = Approvals::default();
    for name in ["web_search", "web_fetch"] {
        assert!(approvals
            .confirm_text_call("run", ApprovalKind::Network, true, name, "{}", &never)
            .await
            .unwrap());
    }
    assert!(!approvals
        .request_web(
            "run",
            "docs.rs",
            "resolved URL".into(),
            &answer(&approvals, ApprovalDecision::Deny)
        )
        .await
        .unwrap());
    approvals
        .request_web(
            "run",
            "docs.rs",
            "resolved URL".into(),
            &answer(&approvals, ApprovalDecision::AllowWebForRun),
        )
        .await
        .unwrap();
    assert!(approvals
        .confirm_text_call(
            "run",
            ApprovalKind::Network,
            true,
            "web_fetch",
            "different args",
            &never
        )
        .await
        .unwrap());
    assert!(approvals
        .request_web("run", "example.org", "different URL".into(), &never)
        .await
        .unwrap());
    assert!(!approvals
        .confirm_text_call(
            "run",
            ApprovalKind::Network,
            false,
            "lookup",
            "{}",
            &answer(&approvals, ApprovalDecision::Deny)
        )
        .await
        .unwrap());
    assert!(!approvals
        .request(
            "run",
            ApprovalKind::Network,
            "example.org",
            "network".into(),
            &answer(&approvals, ApprovalDecision::Deny)
        )
        .await
        .unwrap());
}

#[tokio::test]
async fn cancellation_invalidates_web_grant_handles() {
    let approvals = Approvals::default();
    let (send, receive) = oneshot::channel();
    let sender = Mutex::new(Some(send));
    let emit = |event| {
        sender.lock().unwrap().take().unwrap().send(event).unwrap();
    };
    let mut work = Box::pin(approvals.request_web("run", "docs.rs", "page".into(), &emit));
    let event = tokio::select! {
        result = &mut work => panic!("unexpected result: {result:?}"),
        event = receive => event.unwrap(),
    };
    drop(work);
    let AgentEvent::ApprovalRequest {
        request_id,
        approval_id,
        ..
    } = event
    else {
        panic!("expected approval")
    };
    assert!(approvals
        .resolve(ResolveApprovalRequest {
            request_id,
            approval_id,
            decision: ApprovalDecision::AllowWebForRun
        })
        .is_err());
    assert!(approvals.0.lock().unwrap().web_runs.is_empty());
}

#[tokio::test]
async fn capability_deny_is_not_overridden_by_a_host_allow() {
    let fixture = Fixture::new();
    fixture.write(json!({"rules":[
        {"decision":"allow","kind":"network","host":"docs.rs"},
        {"decision":"deny","kind":"network","tool":policy::WEB_ACCESS_TOOL}
    ]}));
    assert!(!fixture
        .approvals()
        .await
        .request_web("run", "docs.rs", "page".into(), &never)
        .await
        .unwrap());
    // The saved format remains part of the existing validated rules file.
    let _: RulesFile = policy::owner_rules(&fixture.0).unwrap();
}
