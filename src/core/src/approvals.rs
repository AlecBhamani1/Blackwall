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

/// The most argument bytes a text-written call's prompt shows.
const MAX_SHOWN_ARGUMENTS: usize = 4 * 1024;

/// `text` cut to at most `limit` bytes on a character boundary, with a marker when cut.
fn clipped(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let end = (0..=limit)
        .rev()
        .find(|&index| text.is_char_boundary(index))
        .unwrap_or(0);
    format!(
        "{}\n… [{} more bytes not shown]",
        &text[..end],
        text.len() - end
    )
}

/// What a built-in tool does, for the prompt shown before a call the model wrote as text.
pub fn text_call_kind(name: &str) -> ApprovalKind {
    match name {
        "write_file" => ApprovalKind::File,
        "web_fetch" | "web_search" => ApprovalKind::Network,
        _ => ApprovalKind::Exec,
    }
}

struct Pending {
    request_id: String,
    sender: oneshot::Sender<ApprovalDecision>,
    /// Whether the owner may save a rule for this approval.
    tool: bool,
    /// Whether "allow identical action this run" may be remembered.
    remember: bool,
}
#[derive(Default)]
struct Registry {
    pending: HashMap<String, Pending>,
    allowed: HashSet<(String, String)>,
    policies: HashMap<String, Arc<Policy>>,
    /// Runs whose current text-written web call was just approved; its first network prompt
    /// is answered by that approval.
    text_web_calls: HashSet<String>,
}
#[derive(Clone, Default)]
pub struct Approvals(Arc<Mutex<Registry>>);
/// Trusted project settings captured by the same read as the run's project policy.
pub struct LoadedProjectSettings {
    pub warnings: Vec<String>,
    pub limits: Option<crate::project::ProjectLimits>,
    pub instructions: Option<crate::project::InstructionSettings>,
}
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
    /// Apply global owner rules to a run without a selected project.
    pub async fn load_global_policy(
        &self,
        request_id: &str,
        directory: &Path,
    ) -> Result<(), String> {
        let policy = Arc::new(Policy::load_global(directory).await);
        self.0
            .lock()
            .map_err(|_| "The approval registry is unavailable.")?
            .policies
            .insert(request_id.into(), policy);
        Ok(())
    }

    /// Apply the owner's rules in `directory`, and the project's rules, to this run until the
    /// run is cleared. Returns trusted settings from that same project file read.
    pub async fn load_policy(
        &self,
        request_id: &str,
        directory: &Path,
        workspace: &Path,
    ) -> Result<LoadedProjectSettings, String> {
        let policy = Arc::new(Policy::load(directory, workspace).await);
        let settings = LoadedProjectSettings {
            warnings: policy.warnings.clone(),
            limits: policy.project_limits.clone(),
            instructions: policy.project_instructions.clone(),
        };
        self.0
            .lock()
            .map_err(|_| "The approval registry is unavailable.")?
            .policies
            .insert(request_id.into(), policy);
        Ok(settings)
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
        self.decide(request_id, kind, subject, None, None, detail, emit)
            .await
    }
    /// An owner HTTP command tool's request, asked with kind Network; `host` is the subject. See
    /// [`Policy::evaluate_http_tool`] for which rules apply.
    pub async fn request_http_tool(
        &self,
        request_id: &str,
        tool: &str,
        host: &str,
        credentialed: bool,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<bool, String> {
        self.decide(
            request_id,
            ApprovalKind::Network,
            host,
            None,
            Some((tool, credentialed)),
            detail,
            emit,
        )
        .await
    }
    /// A subagent handing its prompt to an owner command tool. It always asks, even when an
    /// allow rule matches `subject` (the tool's command), and the answer is never remembered.
    /// A deny rule still denies without asking: tightening always wins.
    pub async fn request_delegate(
        &self,
        request_id: &str,
        subject: &str,
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
        let verdict = policy
            .as_ref()
            .map(|policy| policy.evaluate(ApprovalKind::Exec, subject));
        if let (Some(policy), Some(verdict)) = (&policy, &verdict) {
            if verdict.decision == Decision::Deny {
                policy
                    .record(
                        ApprovalKind::Exec,
                        subject,
                        Decision::Deny,
                        false,
                        None,
                        verdict.matched,
                    )
                    .await;
                return Ok(false);
            }
        }
        let allowed = self
            .ask(
                request_id,
                ApprovalKind::Exec,
                None,
                detail,
                emit,
                false,
                true,
            )
            .await?
            != ApprovalDecision::Deny;
        if let (Some(policy), Some(verdict)) = (&policy, verdict) {
            policy
                .record(
                    ApprovalKind::Exec,
                    subject,
                    Decision::Ask,
                    allowed,
                    None,
                    verdict.matched,
                )
                .await;
        }
        Ok(allowed)
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
            None,
            detail,
            emit,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn decide(
        &self,
        request_id: &str,
        kind: ApprovalKind,
        subject: &str,
        tool: Option<ApprovalTool>,
        http_tool: Option<(&str, bool)>,
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
            if self.approved_text_call(request_id, kind) {
                return Ok(true);
            }
            // Without the owner's data folder there is nowhere to save a rule.
            let answer = self
                .ask(request_id, kind, None, detail, emit, true, false)
                .await?;
            return Ok(answer != ApprovalDecision::Deny);
        };
        let verdict = match (&tool, http_tool) {
            (Some(_), _) => policy.evaluate_tool(subject),
            (None, Some((name, credentialed))) => {
                policy.evaluate_http_tool(name, subject, credentialed)
            }
            (None, None) => policy.evaluate(kind, subject),
        };
        let decision = verdict.decision;
        let mut saved = None;
        let allowed = match decision {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask if self.approved_text_call(request_id, kind) => true,
            Decision::Ask => {
                let detail = match &policy.error {
                    Some(error) => format!("{error}\n\n{detail}"),
                    None => detail,
                };
                let answer = self
                    .ask(request_id, kind, tool.clone(), detail, emit, true, false)
                    .await?;
                if let Some(tool) = tool.filter(|_| answer != ApprovalDecision::Deny) {
                    match self.save_tool_rule(request_id, &policy, tool, answer).await {
                        Ok(rule) => saved = rule,
                        Err(error) => {
                            policy
                                .record(kind, subject, decision, false, None, verdict.matched)
                                .await;
                            return Err(error);
                        }
                    }
                }
                answer != ApprovalDecision::Deny
            }
        };
        policy
            .record(
                kind,
                subject,
                decision,
                allowed,
                saved.as_ref(),
                verdict.matched,
            )
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
        // A project's server (`p-`) is allowed only in that project, whatever was chosen.
        let in_project = in_project || rule.starts_with("p-");
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
    /// A tool call the model wrote as text always asks, whatever rules are saved, and the
    /// answer is never remembered, not even for an identical call later in the run. `kind` is
    /// what the tool does. The tool's own checks still apply after this. With `covers_request`,
    /// an approved web call does not ask again for its first request; call
    /// [`Self::end_text_call`] after it. Owner HTTP tools pass `false`, so the owner still sees
    /// the prompt with the resolved URL.
    pub async fn confirm_text_call(
        &self,
        request_id: &str,
        kind: ApprovalKind,
        covers_request: bool,
        name: &str,
        arguments: &str,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
    ) -> Result<bool, String> {
        let detail = format!(
            "The model wrote this tool call as text, so Blackwall asks before running it.\n\nTool: {name}\nArguments: {}",
            clipped(arguments, MAX_SHOWN_ARGUMENTS)
        );
        let allowed = self
            .ask(request_id, kind, None, detail, emit, false, false)
            .await?
            != ApprovalDecision::Deny;
        if allowed && covers_request && kind == ApprovalKind::Network {
            self.0
                .lock()
                .map_err(|_| "The approval registry is unavailable.")?
                .text_web_calls
                .insert(request_id.to_owned());
        }
        Ok(allowed)
    }
    /// Drop an approved text-written web call's unused approval once the call has finished.
    pub fn end_text_call(&self, request_id: &str) {
        if let Ok(mut state) = self.0.lock() {
            state.text_web_calls.remove(request_id);
        }
    }
    /// Whether a network prompt is already answered by the text-written call that raised it.
    fn approved_text_call(&self, request_id: &str, kind: ApprovalKind) -> bool {
        kind == ApprovalKind::Network
            && self
                .0
                .lock()
                .is_ok_and(|mut state| state.text_web_calls.remove(request_id))
    }
    /// `remember` lets "allow identical action this run" skip later identical prompts.
    /// `off_device` marks a prompt whose data may leave this device.
    #[allow(clippy::too_many_arguments)]
    async fn ask(
        &self,
        request_id: &str,
        kind: ApprovalKind,
        tool: Option<ApprovalTool>,
        detail: String,
        emit: &(dyn Fn(AgentEvent) + Send + Sync),
        remember: bool,
        off_device: bool,
    ) -> Result<ApprovalDecision, String> {
        let (sender, receiver) = oneshot::channel();
        let id = format!("approval_{:016x}", UnwrapErr(SysRng).next_u64());
        {
            let mut state = self
                .0
                .lock()
                .map_err(|_| "The approval registry is unavailable.")?;
            if remember
                && state
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
                    remember,
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
            remember,
            off_device,
        });
        let decision = tokio::time::timeout(Duration::from_secs(10 * 60), receiver)
            .await
            .map_err(|_| "Approval expired. Ask Blackwall to try again.")?
            .map_err(|_| "Approval was cancelled.")?;
        if remember && decision == ApprovalDecision::AlwaysAllow {
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
            // A prompt that cannot be remembered is allowed once at most.
            let decision = match request.decision {
                ApprovalDecision::AlwaysAllow if !pending.remember => ApprovalDecision::Allow,
                decision => decision,
            };
            pending
                .sender
                .send(decision)
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
            state.text_web_calls.remove(request_id);
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn project_policy_and_limits_share_one_load() {
        let root = std::env::temp_dir().join(format!(
            "blackwall-project-policy-snapshot-{}",
            rand::random::<u64>()
        ));
        let (directory, workspace) = (root.join("data"), root.join("work"));
        std::fs::create_dir_all(workspace.join(crate::project::PROJECT_DIRECTORY)).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let file = workspace
            .join(crate::project::PROJECT_DIRECTORY)
            .join(crate::project::PROJECT_FILE);
        std::fs::write(
            &file,
            json!({
                "policy": {"rules": [{
                    "decision": "deny", "kind": "exec", "command": "snapshot"
                }]},
                "limits": {"max_output_bytes": 4096}
            })
            .to_string(),
        )
        .unwrap();
        let sha = crate::project::status(&workspace, &directory)
            .sha256
            .unwrap();
        crate::project::trust(&workspace, &directory, &sha).unwrap();

        let approvals = Approvals::default();
        let settings = approvals
            .load_policy("run", &directory, &workspace)
            .await
            .unwrap();

        // A later edit cannot split the run between policy from one file version and limits from
        // another, because both values above came from the same loaded snapshot.
        std::fs::write(
            &file,
            json!({"policy": {"rules": []}, "limits": {"max_output_bytes": 8192}}).to_string(),
        )
        .unwrap();
        let sha = crate::project::status(&workspace, &directory)
            .sha256
            .unwrap();
        crate::project::trust(&workspace, &directory, &sha).unwrap();

        assert_eq!(settings.limits.unwrap().max_output_bytes, Some(4096));
        let never = |_| panic!("the loaded deny rule must not ask");
        assert!(!approvals
            .request(
                "run",
                ApprovalKind::Exec,
                "snapshot",
                "detail".into(),
                &never,
            )
            .await
            .unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

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
    async fn p_tool_allow_scoped_to_project() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-p-tool-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let workspace = tokio::fs::canonicalize(&directory).await.unwrap();
        // A hand-written rule allowing a project's server everywhere is ignored.
        tokio::fs::write(
            directory.join(crate::policy::POLICY_FILE),
            r#"{"rules":[{"decision":"allow","kind":"exec","tool":"p-docs"}]}"#,
        )
        .await
        .unwrap();
        let approvals = Approvals::default();
        approvals
            .load_policy("run", &directory, &workspace)
            .await
            .unwrap();
        let asked = Arc::new(Mutex::new(0));
        let count = asked.clone();
        let decisions = approvals.clone();
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = event
            {
                *count.lock().unwrap() += 1;
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id,
                        approval_id,
                        decision: ApprovalDecision::AllowServer,
                    })
                    .unwrap();
            }
        };
        let tool = ApprovalTool {
            server: "p-docs".into(),
            tool: "p-docs__search".into(),
        };
        assert!(approvals
            .request_tool("run", tool, "detail".into(), &emit)
            .await
            .unwrap());
        assert_eq!(*asked.lock().unwrap(), 1);
        // "Always allow this server" is saved for this project only.
        let saved = crate::policy::tool_rules(&directory).unwrap();
        assert!(saved.contains(&crate::policy::ToolRule {
            tool: "p-docs".into(),
            project: Some(workspace.to_string_lossy().into_owned()),
        }));
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
    /// Resolves every prompt with `decision` and keeps each prompt event.
    fn answer_all(
        approvals: &Approvals,
        decision: ApprovalDecision,
    ) -> (
        Arc<Mutex<Vec<AgentEvent>>>,
        impl Fn(AgentEvent) + Send + Sync,
    ) {
        let events = Arc::new(Mutex::new(vec![]));
        let (captured, decisions) = (events.clone(), approvals.clone());
        let emit = move |event: AgentEvent| {
            if let AgentEvent::ApprovalRequest {
                request_id,
                approval_id,
                ..
            } = &event
            {
                decisions
                    .resolve(ResolveApprovalRequest {
                        request_id: request_id.clone(),
                        approval_id: approval_id.clone(),
                        decision,
                    })
                    .unwrap();
            }
            captured.lock().unwrap().push(event);
        };
        (events, emit)
    }
    #[tokio::test]
    async fn text_call_prompts_are_never_remembered_and_show_their_kind() {
        let approvals = Approvals::default();
        let (events, emit) = answer_all(&approvals, ApprovalDecision::AlwaysAllow);
        let arguments = format!(r#"{{"path":"a.txt","content":"{}"}}"#, "x".repeat(20_000));
        for _ in 0..2 {
            assert!(approvals
                .confirm_text_call(
                    "run",
                    ApprovalKind::File,
                    true,
                    "write_file",
                    &arguments,
                    &emit
                )
                .await
                .unwrap());
        }
        let events = events.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "an identical text call asks again");
        let AgentEvent::ApprovalRequest {
            kind,
            detail,
            remember,
            ..
        } = &events[0]
        else {
            panic!("{events:?}")
        };
        assert_eq!(*kind, ApprovalKind::File);
        assert!(!remember);
        assert!(detail.len() < 5_000, "{}", detail.len());
        assert!(detail.contains("more bytes not shown"), "{detail}");
        // Ordinary prompts can still be remembered.
        let (events, emit) = answer_all(&approvals, ApprovalDecision::Allow);
        approvals
            .request("run", ApprovalKind::Exec, "ls", "ls".into(), &emit)
            .await
            .unwrap();
        assert!(matches!(
            events.lock().unwrap()[0],
            AgentEvent::ApprovalRequest { remember: true, .. }
        ));
    }
    #[tokio::test]
    async fn approved_text_web_call_skips_its_own_network_prompt_once() {
        let approvals = Approvals::default();
        let (_, allow) = answer_all(&approvals, ApprovalDecision::Allow);
        let arguments = r#"{"url":"https://docs.rs"}"#;
        assert!(approvals
            .confirm_text_call(
                "run",
                ApprovalKind::Network,
                true,
                "web_fetch",
                arguments,
                &allow
            )
            .await
            .unwrap());
        assert!(approvals
            .request(
                "run",
                ApprovalKind::Network,
                "docs.rs",
                "GET https://docs.rs".into(),
                &|_| panic!("the approved text call already asked")
            )
            .await
            .unwrap());
        // The approval covered one request only.
        let (_, deny) = answer_all(&approvals, ApprovalDecision::Deny);
        assert!(!approvals
            .request(
                "run",
                ApprovalKind::Network,
                "docs.rs",
                "GET https://docs.rs".into(),
                &deny
            )
            .await
            .unwrap());
        // An unused approval ends with its call.
        approvals
            .confirm_text_call(
                "run",
                ApprovalKind::Network,
                true,
                "web_fetch",
                arguments,
                &allow,
            )
            .await
            .unwrap();
        approvals.end_text_call("run");
        assert!(!approvals
            .request(
                "run",
                ApprovalKind::Network,
                "docs.rs",
                "GET https://docs.rs".into(),
                &deny
            )
            .await
            .unwrap());
        // A denied text call grants nothing.
        approvals
            .confirm_text_call(
                "run",
                ApprovalKind::Network,
                true,
                "web_fetch",
                arguments,
                &deny,
            )
            .await
            .unwrap();
        assert!(!approvals
            .request(
                "run",
                ApprovalKind::Network,
                "docs.rs",
                "GET https://docs.rs".into(),
                &deny
            )
            .await
            .unwrap());
    }
    #[test]
    fn approving_a_text_call_always_allows_only_once() {
        let approvals = Approvals::default();
        let (sender, mut receiver) = oneshot::channel();
        approvals.0.lock().unwrap().pending.insert(
            "a1".into(),
            Pending {
                request_id: "run".into(),
                sender,
                tool: false,
                remember: false,
            },
        );
        approvals
            .resolve(ResolveApprovalRequest {
                request_id: "run".into(),
                approval_id: "a1".into(),
                decision: ApprovalDecision::AlwaysAllow,
            })
            .unwrap();
        assert_eq!(receiver.try_recv().unwrap(), ApprovalDecision::Allow);
    }
}
