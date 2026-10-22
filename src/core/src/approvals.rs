//! Exact-action approvals tied to a live run. Dropping a wait invalidates its decision handle.
use crate::{
    policy::{Decision, Policy, ToolRule},
    protocol::{AgentEvent, ApprovalDecision, ApprovalKind, ApprovalTool, ResolveApprovalRequest},
};
use rand::{rand_core::UnwrapErr, rngs::SysRng, Rng};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

struct Pending {
    request_id: String,
    sender: oneshot::Sender<ApprovalDecision>,
    /// Whether the owner may save a rule for this approval.
    tool: bool,
}
#[derive(Default)]
struct Registry {
    pending: HashMap<String, Pending>,
    allowed: HashSet<(String, String)>,
    policies: HashMap<String, Arc<Policy>>,
}
#[derive(Clone, Default)]
pub struct Approvals(Arc<Mutex<Registry>>);
struct Waiting {
    approvals: Approvals,
    id: String,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        if let Ok(mut state) = self.approvals.0.lock() {
            state.pending.remove(&self.id);
        }
    }
}
impl Approvals {
    /// Apply the owner's rules in `directory` to this run until the run is cleared.
    pub async fn load_policy(
        &self,
        request_id: &str,
        directory: &Path,
        workspace: &Path,
    ) -> Result<(), String> {
        let policy = Arc::new(Policy::load(directory, workspace).await);
        self.0
            .lock()
            .map_err(|_| "The approval registry is unavailable.")?
            .policies
            .insert(request_id.into(), policy);
        Ok(())
    }
    /// `subject` is the command, workspace-relative path, or host that rules match.
    pub async fn request(
        &self,
        request_id: &str,
        kind: ApprovalKind,
        subject: &str,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<bool, String> {
        self.decide(request_id, kind, subject, None, detail, emit)
            .await
    }
    /// MCP calls match `tool` rules, and the owner can save one from the prompt.
    pub async fn request_tool(
        &self,
        request_id: &str,
        tool: ApprovalTool,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<bool, String> {
        let subject = tool.tool.clone();
        self.decide(
            request_id,
            ApprovalKind::Exec,
            &subject,
            Some(tool),
            detail,
            emit,
        )
        .await
    }
    async fn decide(
        &self,
        request_id: &str,
        kind: ApprovalKind,
        subject: &str,
        tool: Option<ApprovalTool>,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<bool, String> {
        let policy = self
            .0
            .lock()
            .map_err(|_| "The approval registry is unavailable.")?
            .policies
            .get(request_id)
            .cloned();
        let Some(policy) = policy else {
            // Without the owner's data folder there is nowhere to save a rule.
            let answer = self.ask(request_id, kind, None, detail, emit).await?;
            return Ok(answer != ApprovalDecision::Deny);
        };
        let decision = match tool {
            Some(_) => policy.decide_tool(subject),
            None => policy.decide(kind, subject),
        };
        let mut saved = None;
        let allowed = match decision {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask => {
                let detail = match &policy.error {
                    Some(error) => format!("{error}\n\n{detail}"),
                    None => detail,
                };
                let answer = self
                    .ask(request_id, kind, tool.clone(), detail, emit)
                    .await?;
                if let Some(tool) = tool.filter(|_| answer != ApprovalDecision::Deny) {
                    match self.save_tool_rule(request_id, &policy, tool, answer).await {
                        Ok(rule) => saved = rule,
                        Err(error) => {
                            policy.record(kind, subject, decision, false, None).await;
                            return Err(error);
                        }
                    }
                }
                answer != ApprovalDecision::Deny
            }
        };
        policy
            .record(kind, subject, decision, allowed, saved.as_ref())
            .await;
        Ok(allowed)
    }
    /// Save the rule the owner chose and apply it to the rest of the run. A rule that cannot be
    /// saved stops the call, so the owner is not left believing it was remembered.
    async fn save_tool_rule(
        &self,
        request_id: &str,
        policy: &Policy,
        tool: ApprovalTool,
        answer: ApprovalDecision,
    ) -> Result<Option<ToolRule>, String> {
        let (rule, in_project) = match answer {
            ApprovalDecision::AllowToolInProject => (tool.tool, true),
            ApprovalDecision::AllowTool => (tool.tool, false),
            ApprovalDecision::AllowServerInProject => (tool.server, true),
            ApprovalDecision::AllowServer => (tool.server, false),
            _ => return Ok(None),
        };
        let (directory, workspace) = policy.scope();
        let directory = directory.to_path_buf();
        let project = in_project.then(|| workspace.to_path_buf());
        let saved = ToolRule {
            tool: rule.clone(),
            project: project
                .as_ref()
                .map(|project| project.to_string_lossy().into_owned()),
        };
        tokio::task::spawn_blocking(move || {
            crate::policy::allow_tool(&directory, &rule, project.as_deref())
        })
        .await
        .map_err(|_| "The policy file could not be saved.".to_owned())?
        .map_err(|error| format!("{error} The tool was not run."))?;
        let reloaded = Arc::new(policy.reload().await);
        if let Some(current) = self
            .0
            .lock()
            .map_err(|_| "The approval registry is unavailable.")?
            .policies
            .get_mut(request_id)
        {
            *current = reloaded;
        }
        Ok(Some(saved))
    }
    async fn ask(
        &self,
        request_id: &str,
        kind: ApprovalKind,
        tool: Option<ApprovalTool>,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<ApprovalDecision, String> {
        let (sender, receiver) = oneshot::channel();
        let id = format!("approval_{:016x}", UnwrapErr(SysRng).next_u64());
        {
            let mut state = self
                .0
                .lock()
                .map_err(|_| "The approval registry is unavailable.")?;
            if state
                .allowed
                .contains(&(request_id.to_owned(), detail.clone()))
            {
                return Ok(ApprovalDecision::Allow);
            }
            if state.pending.len() >= 16 {
                return Err("Too many approvals are waiting.".into());
            }
            state.pending.insert(
                id.clone(),
                Pending {
                    request_id: request_id.into(),
                    sender,
                    tool: tool.is_some(),
                },
            );
        }
        let _guard = Waiting {
            approvals: self.clone(),
            id: id.clone(),
        };
        emit(AgentEvent::ApprovalRequest {
            request_id: request_id.into(),
            approval_id: id,
            kind,
            detail: detail.clone(),
            tool,
        });
        let decision = tokio::time::timeout(Duration::from_secs(10 * 60), receiver)
            .await
            .map_err(|_| "Approval expired. Ask Blackwall to try again.")?
            .map_err(|_| "Approval was cancelled.")?;
        if decision == ApprovalDecision::AlwaysAllow {
            self.0
                .lock()
                .map_err(|_| "The approval registry is unavailable.")?
                .allowed
                .insert((request_id.into(), detail));
        }
        Ok(decision)
    }
    pub fn resolve(&self, request: ResolveApprovalRequest) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "The approval registry is unavailable.")?;
        let Some(pending) = state
            .pending
            .get(&request.approval_id)
            .filter(|pending| pending.request_id == request.request_id)
        else {
            return Err("This approval is no longer active.".into());
        };
        let saves_rule = matches!(
            request.decision,
            ApprovalDecision::AllowToolInProject
                | ApprovalDecision::AllowTool
                | ApprovalDecision::AllowServerInProject
                | ApprovalDecision::AllowServer
        );
        if saves_rule && !pending.tool {
            return Err("This approval cannot be saved as a rule.".into());
        }
        if let Some(pending) = state.pending.remove(&request.approval_id) {
            pending
                .sender
                .send(request.decision)
                .map_err(|_| "This approval is no longer active.")?;
        }
        Ok(())
    }
    pub fn clear_run(&self, request_id: &str) {
        if let Ok(mut state) = self.0.lock() {
            state
                .pending
                .retain(|_, pending| pending.request_id != request_id);
            state.allowed.retain(|(id, _)| id != request_id);
            state.policies.remove(request_id);
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn wrong_run_cannot_authorize_and_dropped_wait_is_revoked() {
        let approvals = Approvals::default();
        let events = Arc::new(Mutex::new(vec![]));
        let captured = events.clone();
        let emit = move |e| captured.lock().unwrap().push(e);
        let wait = approvals.request(
            "run1",
            ApprovalKind::Exec,
            "echo test",
            "echo test".into(),
            &emit,
        );
        tokio::pin!(wait);
        tokio::select! { _=&mut wait => panic!("must await decision"), _=tokio::time::sleep(Duration::from_millis(1)) => {} }
        let event = events.lock().unwrap()[0].clone();
        if let AgentEvent::ApprovalRequest { approval_id, .. } = event {
            assert!(approvals
                .resolve(ResolveApprovalRequest {
                    request_id: "wrong".into(),
                    approval_id: approval_id.clone(),
                    decision: ApprovalDecision::Allow
                })
                .is_err());
            assert!(approvals
                .resolve(ResolveApprovalRequest {
                    request_id: "run1".into(),
                    approval_id: approval_id.clone(),
                    decision: ApprovalDecision::AllowServer
                })
                .is_err());
            approvals.clear_run("run1");
            assert!(approvals
                .resolve(ResolveApprovalRequest {
                    request_id: "run1".into(),
                    approval_id,
                    decision: ApprovalDecision::Allow
                })
                .is_err());
        }
    }
    #[tokio::test]
    async fn saved_tool_rules_apply_to_the_rest_of_the_run() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-always-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let workspace = tokio::fs::canonicalize(&directory).await.unwrap();
        let approvals = Approvals::default();
        approvals
            .load_policy("run", &directory, &workspace)
            .await
            .unwrap();
        let tool = |name: &str| ApprovalTool {
            server: "docs".into(),
            tool: name.into(),
        };
        let events = Arc::new(Mutex::new(vec![]));
        let captured = events.clone();
        let decisions = approvals.clone();
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                tool,
                ..
            } = &event
            {
                assert_eq!(tool.as_ref().unwrap().server, "docs");
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id: request_id.clone(),
                        approval_id: approval_id.clone(),
                        decision: ApprovalDecision::AllowServerInProject,
                    })
                    .unwrap();
            }
            captured.lock().unwrap().push(event);
        };
        assert!(approvals
            .request_tool("run", tool("docs__search"), "detail".into(), &emit)
            .await
            .unwrap());
        // The saved rule covers the server's other tools without another prompt.
        assert!(approvals
            .request_tool("run", tool("docs__fetch"), "other".into(), &emit)
            .await
            .unwrap());
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(
            crate::policy::tool_rules(&directory).unwrap(),
            vec![crate::policy::ToolRule {
                tool: "docs".into(),
                project: Some(workspace.to_string_lossy().into_owned()),
            }]
        );
        let audit = tokio::fs::read_to_string(directory.join("policy-audit.jsonl"))
            .await
            .unwrap();
        assert!(audit.contains(r#""saved":{"project""#), "{audit}");
        // A rule that cannot be saved stops the call instead of running it unremembered.
        approvals.clear_run("run");
        tokio::fs::write(directory.join(crate::policy::POLICY_FILE), "not json")
            .await
            .unwrap();
        approvals
            .load_policy("run", &directory, &workspace)
            .await
            .unwrap();
        let error = approvals
            .request_tool("run", tool("docs__search"), "again".into(), &emit)
            .await
            .unwrap_err();
        assert!(error.contains("The tool was not run."), "{error}");
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }
    #[tokio::test]
    async fn owner_rules_decide_before_prompting_and_end_with_the_run() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-rules-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        tokio::fs::write(
            directory.join(crate::policy::POLICY_FILE),
            r#"{"rules":[
                {"decision":"allow","kind":"exec","command":"cargo test"},
                {"decision":"deny","kind":"file","path":".git"}
            ]}"#,
        )
        .await
        .unwrap();
        let approvals = Approvals::default();
        approvals
            .load_policy("run", &directory, Path::new("/workspace"))
            .await
            .unwrap();
        let never = |_| panic!("a matched rule must not prompt");
        assert!(approvals
            .request(
                "run",
                ApprovalKind::Exec,
                "cargo test",
                "detail".into(),
                &never
            )
            .await
            .unwrap());
        assert!(!approvals
            .request(
                "run",
                ApprovalKind::File,
                ".git/HEAD",
                "detail".into(),
                &never
            )
            .await
            .unwrap());
        let audit = tokio::fs::read_to_string(directory.join("policy-audit.jsonl"))
            .await
            .unwrap();
        assert_eq!(audit.lines().count(), 2);
        approvals.clear_run("run");
        let prompts = Arc::new(Mutex::new(0));
        let counted = prompts.clone();
        let count = move |_| *counted.lock().unwrap() += 1;
        let wait = approvals.request(
            "run",
            ApprovalKind::Exec,
            "cargo test",
            "detail".into(),
            &count,
        );
        tokio::pin!(wait);
        tokio::select! { _=&mut wait => panic!("must ask after the run ends"), _=tokio::time::sleep(Duration::from_millis(1)) => {} }
        assert_eq!(*prompts.lock().unwrap(), 1);

        tokio::fs::write(directory.join(crate::policy::POLICY_FILE), "{")
            .await
            .unwrap();
        approvals
            .load_policy("broken", &directory, Path::new("/workspace"))
            .await
            .unwrap();
        let details = Arc::new(Mutex::new(vec![]));
        let captured = details.clone();
        let capture = move |event| {
            if let AgentEvent::ApprovalRequest { detail, .. } = event {
                captured.lock().unwrap().push(detail);
            }
        };
        let wait = approvals.request(
            "broken",
            ApprovalKind::Exec,
            "cargo test",
            "detail".into(),
            &capture,
        );
        tokio::pin!(wait);
        tokio::select! { _=&mut wait => panic!("an invalid policy must ask"), _=tokio::time::sleep(Duration::from_millis(1)) => {} }
        assert!(details.lock().unwrap()[0].starts_with("The policy file is not valid."));
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }
    #[tokio::test]
    async fn remembered_permission_is_exact_and_expires_with_run() {
        let approvals = Approvals::default();
        let decisions = approvals.clone();
        let allow = move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::AlwaysAllow,
                    })
                    .unwrap();
            }
        };
        assert!(approvals
            .request(
                "one",
                ApprovalKind::Exec,
                "exact command",
                "exact command".into(),
                &allow
            )
            .await
            .unwrap());
        assert!(approvals
            .request(
                "one",
                ApprovalKind::Exec,
                "exact command",
                "exact command".into(),
                &|_| panic!("identical action should use the run permission")
            )
            .await
            .unwrap());
        let decisions = approvals.clone();
        let deny = move |event| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::Deny,
                    })
                    .unwrap();
            }
        };
        assert!(!approvals
            .request(
                "one",
                ApprovalKind::Exec,
                "different command",
                "different command".into(),
                &deny
            )
            .await
            .unwrap());
        approvals.clear_run("one");
        assert!(!approvals
            .request(
                "two",
                ApprovalKind::Exec,
                "exact command",
                "exact command".into(),
                &deny
            )
            .await
            .unwrap());
    }
}
