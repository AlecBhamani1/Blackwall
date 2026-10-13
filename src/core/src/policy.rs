//! Owner-defined allow, ask, and deny rules evaluated before approval prompts.
use crate::protocol::ApprovalKind;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const POLICY_FILE: &str = "policy.json";
const AUDIT_FILE: &str = "policy-audit.jsonl";
const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_RULES: usize = 200;
const MAX_AUDIT_BYTES: u64 = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    decision: Decision,
    kind: ApprovalKind,
    command: Option<String>,
    path: Option<String>,
    host: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    rules: Vec<Rule>,
}

/// Rules loaded for one run. A missing file has no rules; an invalid file has no rules and an
/// error, so every action keeps asking.
#[derive(Debug)]
pub struct Policy {
    rules: Vec<Rule>,
    pub error: Option<String>,
    audit: PathBuf,
    /// The data folder relative to the workspace, when the workspace contains it.
    protected: Option<PathBuf>,
}

impl Policy {
    pub async fn load(directory: &Path, workspace: &Path) -> Self {
        let audit = directory.join(AUDIT_FILE);
        let protected = tokio::fs::canonicalize(directory)
            .await
            .ok()
            .and_then(|data| {
                let relative = data.strip_prefix(workspace).ok()?;
                normalize(&relative.to_string_lossy().to_lowercase())
            });
        let path = directory.join(POLICY_FILE);
        let parsed = match tokio::time::timeout(IO_TIMEOUT, read_bounded(&path))
            .await
            .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(_) => Err("The policy file could not be read."),
            Ok(body) if body.len() > MAX_POLICY_BYTES => Err("The policy file is too large."),
            Ok(body) => serde_json::from_slice::<RuleFile>(&body)
                .map_err(|_| "The policy file is not valid.")
                .and_then(|file| validate(file.rules)),
        };
        match parsed {
            Ok(rules) => Self {
                rules,
                error: None,
                audit,
                protected,
            },
            Err(error) => Self {
                rules: vec![],
                error: Some(format!("{error} Every action will ask for approval.")),
                audit,
                protected,
            },
        }
    }

    /// Deny outranks ask, and ask outranks allow. Unmatched actions ask, and so do writes to the
    /// data folder, so the agent cannot change its own rules.
    pub fn decide(&self, kind: ApprovalKind, subject: &str) -> Decision {
        let decision = self
            .rules
            .iter()
            .filter(|rule| rule.kind == kind && matches(rule, subject))
            .map(|rule| rule.decision)
            .max()
            .unwrap_or(Decision::Ask);
        let protected = kind == ApprovalKind::File
            && self.protected.as_ref().is_some_and(|data| {
                normalize(&subject.to_lowercase()).is_none_or(|path| path.starts_with(data))
            });
        if protected {
            decision.max(Decision::Ask)
        } else {
            decision
        }
    }

    /// Append a bounded, redacted record. Audit failures never change the decision.
    pub async fn record(
        &self,
        kind: ApprovalKind,
        subject: &str,
        decision: Decision,
        allowed: bool,
    ) {
        let _ =
            tokio::time::timeout(IO_TIMEOUT, self.append(kind, subject, decision, allowed)).await;
    }

    async fn append(&self, kind: ApprovalKind, subject: &str, decision: Decision, allowed: bool) {
        let path = &self.audit;
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let line = format!(
            "{}\n",
            json!({"time":time,"kind":kind,"subject":redact(subject),"decision":decision,"allowed":allowed})
        );
        if tokio::fs::metadata(path)
            .await
            .is_ok_and(|meta| meta.len() > MAX_AUDIT_BYTES)
        {
            // Windows cannot rename over an existing file.
            let backup = path.with_extension("jsonl.1");
            let _ = tokio::fs::remove_file(&backup).await;
            let _ = tokio::fs::rename(path, backup).await;
        }
        if let Ok(mut file) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
        {
            let _ = file.write_all(line.as_bytes()).await;
        }
    }
}

async fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(MAX_POLICY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .await?;
    Ok(body)
}

fn validate(rules: Vec<Rule>) -> Result<Vec<Rule>, &'static str> {
    if rules.len() > MAX_RULES {
        return Err("The policy file has too many rules.");
    }
    for rule in &rules {
        let field = match rule.kind {
            ApprovalKind::Exec => rule.command.as_deref().filter(|c| !c.trim().is_empty()),
            ApprovalKind::File => rule.path.as_deref().filter(|p| normalize(p).is_some()),
            ApprovalKind::Network => rule.host.as_deref().filter(|h| !h.trim().is_empty()),
            ApprovalKind::Subagent => None,
        };
        let fields = [&rule.command, &rule.path, &rule.host]
            .iter()
            .filter(|f| f.is_some())
            .count();
        if field.is_none() || fields != 1 {
            return Err("A policy rule has missing or mismatched fields.");
        }
    }
    Ok(rules)
}

fn matches(rule: &Rule, subject: &str) -> bool {
    match rule.kind {
        ApprovalKind::Exec => rule.command.as_deref().is_some_and(|command| {
            let mut words = subject.split_whitespace();
            command
                .split_whitespace()
                .all(|word| words.next() == Some(word))
                && (rule.decision != Decision::Allow || !has_control_syntax(subject))
        }),
        ApprovalKind::File => {
            // Default macOS and Windows file systems ignore case, so restricting rules do too.
            let fold = |path: &str| match rule.decision {
                Decision::Allow => path.to_owned(),
                Decision::Ask | Decision::Deny => path.to_lowercase(),
            };
            match (
                rule.path
                    .as_deref()
                    .map(fold)
                    .as_deref()
                    .and_then(normalize),
                normalize(&fold(subject)),
            ) {
                (Some(prefix), Some(path)) => path.starts_with(prefix),
                _ => false,
            }
        }
        ApprovalKind::Network => rule.host.as_deref().is_some_and(|host| {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            let subject = subject.trim_end_matches('.').to_ascii_lowercase();
            match host.strip_prefix("*.") {
                Some(domain) => subject.ends_with(&format!(".{domain}")),
                None => subject == host,
            }
        }),
        ApprovalKind::Subagent => false,
    }
}

/// Commands that chain, substitute, expand, or redirect can do more than their first words say.
/// Parentheses evaluate subexpressions in PowerShell arguments, and `\r` ends a PowerShell line.
fn has_control_syntax(command: &str) -> bool {
    command.contains(['\n', '\r', ';', '&', '|', '`', '$', '(', '<', '>'])
}

/// Workspace-relative path with `..` resolved lexically; escapes and absolute paths are `None`.
fn normalize(path: &str) -> Option<PathBuf> {
    let mut normal = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normal.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normal.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normal)
}

fn redact(subject: &str) -> String {
    let mut hide_next = false;
    subject
        .split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            let secret = hide_next;
            hide_next = lower.ends_with("bearer")
                || lower.ends_with("authorization:")
                || ["-u", "--user", "--password", "--token", "--api-key"].contains(&lower.as_str());
            if secret {
                "[redacted]".to_owned()
            } else if let Some((name, _)) = word.split_once('=').filter(|(name, _)| {
                ["key", "token", "secret", "password"]
                    .iter()
                    .any(|hint| name.to_ascii_lowercase().contains(hint))
            }) {
                format!("{name}=[redacted]")
            } else if let Some((scheme, rest)) = word.split_once("://") {
                rest.split_once('@').map_or_else(
                    || word.to_owned(),
                    |(_, host)| format!("{scheme}://[redacted]@{host}"),
                )
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn policy(body: &str) -> Policy {
        Policy {
            rules: validate(serde_json::from_str::<RuleFile>(body).unwrap().rules).unwrap(),
            error: None,
            audit: std::env::temp_dir().join(AUDIT_FILE),
            protected: Some(PathBuf::from(".blackwall")),
        }
    }

    #[test]
    fn deny_outranks_ask_and_allow_and_unmatched_actions_ask() {
        let rules = policy(
            r#"{"rules":[
                {"decision":"allow","kind":"exec","command":"cargo test"},
                {"decision":"ask","kind":"exec","command":"cargo test --release"},
                {"decision":"deny","kind":"file","path":".git"},
                {"decision":"allow","kind":"file","path":"."},
                {"decision":"allow","kind":"network","host":"*.docs.rs"}
            ]}"#,
        );
        assert_eq!(
            rules.decide(ApprovalKind::Exec, "cargo test -p core"),
            Decision::Allow
        );
        assert_eq!(
            rules.decide(ApprovalKind::Exec, "cargo test --release"),
            Decision::Ask
        );
        assert_eq!(
            rules.decide(ApprovalKind::Exec, "cargo testing"),
            Decision::Ask
        );
        assert_eq!(
            rules.decide(ApprovalKind::File, ".git/config"),
            Decision::Deny
        );
        assert_eq!(
            rules.decide(ApprovalKind::File, "src/main.rs"),
            Decision::Allow
        );
        assert_eq!(
            rules.decide(ApprovalKind::Network, "serde.docs.rs"),
            Decision::Allow
        );
        assert_eq!(
            rules.decide(ApprovalKind::Network, "serde.docs.rs."),
            Decision::Allow
        );
        assert_eq!(
            rules.decide(ApprovalKind::Network, "docs.rs.evil.example"),
            Decision::Ask
        );
        assert_eq!(rules.decide(ApprovalKind::Subagent, "child"), Decision::Ask);
    }

    #[test]
    fn control_syntax_and_path_traversal_never_auto_allow() {
        let rules = policy(
            r#"{"rules":[
                {"decision":"allow","kind":"exec","command":"npm test"},
                {"decision":"deny","kind":"file","path":".git"},
                {"decision":"allow","kind":"file","path":"src"}
            ]}"#,
        );
        for command in [
            "npm test; rm -rf .",
            "npm test && curl x",
            "npm test | sh",
            "npm test $(id)",
            "npm test `id`",
            "npm test > out",
            "npm test\nrm -rf .",
            "npm test\rRemove-Item -Recurse ~",
            "npm test (Remove-Item -Recurse ~)",
            "npm test $env:HOME",
        ] {
            assert_eq!(
                rules.decide(ApprovalKind::Exec, command),
                Decision::Ask,
                "{command}"
            );
        }
        assert_eq!(
            rules.decide(ApprovalKind::File, "src/../.git/config"),
            Decision::Deny
        );
        assert_eq!(
            rules.decide(ApprovalKind::File, "SRC/../.Git/config"),
            Decision::Deny
        );
        assert_eq!(
            rules.decide(ApprovalKind::File, "SRC/main.rs"),
            Decision::Ask
        );
        assert_eq!(
            policy(r#"{"rules":[{"decision":"allow","kind":"file","path":"."}]}"#)
                .decide(ApprovalKind::File, ".Blackwall/policy.json"),
            Decision::Ask
        );
        assert_eq!(
            rules.decide(ApprovalKind::File, "src/../../outside"),
            Decision::Ask
        );
        assert_eq!(
            rules.decide(ApprovalKind::File, "/etc/passwd"),
            Decision::Ask
        );
    }

    #[tokio::test]
    async fn invalid_or_missing_files_ask_and_audit_is_redacted() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-policy-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let missing = Policy::load(&directory, Path::new("/workspace")).await;
        assert!(missing.error.is_none());
        assert_eq!(missing.decide(ApprovalKind::Exec, "ls"), Decision::Ask);
        for body in [
            "not json",
            r#"{"rules":[{"decision":"allow","kind":"exec"}]}"#,
            r#"{"rules":[{"decision":"allow","kind":"exec","command":"ls","host":"x"}]}"#,
            r#"{"rules":[{"decision":"allow","kind":"file","path":"../up"}]}"#,
            r#"{"rules":[],"extra":true}"#,
            r#"{"rules":[{"decision":"allow","kind":"subagent"}]}"#,
        ] {
            tokio::fs::write(directory.join(POLICY_FILE), body)
                .await
                .unwrap();
            let invalid = Policy::load(&directory, Path::new("/workspace")).await;
            assert!(invalid.error.is_some(), "{body}");
            assert_eq!(invalid.decide(ApprovalKind::Exec, "ls"), Decision::Ask);
        }
        record_secret(&missing).await;
        let audit = tokio::fs::read_to_string(directory.join(AUDIT_FILE))
            .await
            .unwrap();
        assert!(audit.contains("curl -H Authorization: [redacted]"));
        assert!(audit.contains("API_TOKEN=[redacted]"));
        assert!(audit.contains("https://[redacted]@host"));
        for secret in ["hunter2", "abc123", "xyz789", "pw4"] {
            assert!(!audit.contains(secret), "{secret}");
        }
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    async fn record_secret(policy: &Policy) {
        policy
            .record(
                ApprovalKind::Exec,
                "curl -H Authorization: abc123 -H Authorization:Bearer xyz789 -u me:pw4 \
                 https://me:hunter2@host API_TOKEN=hunter2",
                Decision::Ask,
                false,
            )
            .await;
    }
}
