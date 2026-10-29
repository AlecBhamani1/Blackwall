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

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    decision: Decision,
    kind: ApprovalKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    /// An MCP server name, matching all of its tools, or one `server__tool` name.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    /// Limits the rule to runs in this project folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    rules: Vec<Rule>,
}

/// A saved rule that allows an MCP server or tool, as listed for the owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRule {
    pub tool: String,
    pub project: Option<String>,
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
    directory: PathBuf,
    workspace: PathBuf,
}

impl Policy {
    /// Chat has no project scope. Only the owner's global rules may authorize web requests.
    pub async fn load_global(directory: &Path) -> Self {
        let mut policy = Self::load(directory, directory).await;
        policy.rules.retain(|rule| rule.project.is_none());
        policy
    }

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
        let parsed = match tokio::time::timeout(IO_TIMEOUT, read_bounded(&path, MAX_POLICY_BYTES))
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
        let (rules, error) = match parsed {
            Ok(rules) => (rules, None),
            Err(error) => (
                vec![],
                Some(format!("{error} Every action will ask for approval.")),
            ),
        };
        // Project rules apply only in their own folder. The workspace path is already canonical.
        let mut kept = Vec::with_capacity(rules.len());
        for rule in rules {
            if let Some(project) = &rule.project {
                let folder =
                    tokio::time::timeout(IO_TIMEOUT, tokio::fs::canonicalize(project)).await;
                if folder.ok().and_then(Result::ok).as_deref() != Some(workspace) {
                    continue;
                }
            }
            kept.push(rule);
        }
        Self {
            rules: kept,
            error,
            audit,
            protected,
            directory: directory.into(),
            workspace: workspace.into(),
        }
    }

    /// Reload the owner's rules for the same data folder and workspace.
    pub async fn reload(&self) -> Self {
        Self::load(&self.directory, &self.workspace).await
    }

    /// The owner's data folder and the run's workspace, for saving a rule from an approval.
    pub fn scope(&self) -> (&Path, &Path) {
        (&self.directory, &self.workspace)
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

    /// MCP calls match `tool` rules by server or exact name, and `command` rules by the namespaced
    /// name. `tool` rules never match shell commands, which `decide` handles.
    pub fn decide_tool(&self, exposed: &str) -> Decision {
        let server = exposed
            .split_once("__")
            .map_or(exposed, |(server, _)| server);
        self.rules
            .iter()
            .filter(|rule| {
                rule.kind == ApprovalKind::Exec
                    && match rule.tool.as_deref() {
                        Some(tool) => tool == exposed || tool == server,
                        None => matches(rule, exposed),
                    }
            })
            .map(|rule| rule.decision)
            .max()
            .unwrap_or(Decision::Ask)
    }

    /// Append a bounded, redacted record. Audit failures never change the decision.
    /// `saved` is the rule the owner saved from this approval, if any.
    pub async fn record(
        &self,
        kind: ApprovalKind,
        subject: &str,
        decision: Decision,
        allowed: bool,
        saved: Option<&ToolRule>,
    ) {
        let _ = tokio::time::timeout(
            IO_TIMEOUT,
            self.append(kind, subject, decision, allowed, saved),
        )
        .await;
    }

    async fn append(
        &self,
        kind: ApprovalKind,
        subject: &str,
        decision: Decision,
        allowed: bool,
        saved: Option<&ToolRule>,
    ) {
        let path = &self.audit;
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut entry = json!({"time":time,"kind":kind,"subject":redact(subject),"decision":decision,"allowed":allowed});
        if let Some(rule) = saved {
            entry["saved"] = json!(rule);
        }
        let line = format!("{entry}\n");
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
            // Tokio finishes file writes in the background; flush so the record is on disk
            // before the decision returns.
            if file.write_all(line.as_bytes()).await.is_ok() {
                let _ = file.flush().await;
            }
        }
    }
}

/// Read at most `limit + 1` bytes, so callers can reject an oversized owner file.
pub(crate) async fn read_bounded(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(limit as u64 + 1)
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
            ApprovalKind::Exec => rule
                .command
                .as_deref()
                .filter(|c| !c.trim().is_empty())
                .or(rule
                    .tool
                    .as_deref()
                    .filter(|t| crate::mcp::valid_tool_rule(t))),
            ApprovalKind::File => rule.path.as_deref().filter(|p| normalize(p).is_some()),
            ApprovalKind::Network => rule.host.as_deref().filter(|h| !h.trim().is_empty()),
            ApprovalKind::Subagent => None,
        };
        let fields = [&rule.command, &rule.path, &rule.host, &rule.tool]
            .iter()
            .filter(|f| f.is_some())
            .count();
        if field.is_none()
            || fields != 1
            || rule
                .project
                .as_deref()
                .is_some_and(|project| !Path::new(project).is_absolute())
        {
            return Err("A policy rule has missing or mismatched fields.");
        }
    }
    Ok(rules)
}

/// Save an allow rule for an MCP server or tool, optionally limited to one project, keeping the
/// owner's other rules. An unreadable or invalid file is left unchanged. Blocking.
pub fn allow_tool(directory: &Path, tool: &str, project: Option<&Path>) -> Result<(), String> {
    let rule = Rule {
        decision: Decision::Allow,
        kind: ApprovalKind::Exec,
        command: None,
        path: None,
        host: None,
        tool: Some(tool.into()),
        project: project.map(|project| project.to_string_lossy().into_owned()),
    };
    edit_rules(directory, |rules| {
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    })
}

/// The owner's saved allow rules for MCP servers and tools. Blocking.
pub fn tool_rules(directory: &Path) -> Result<Vec<ToolRule>, String> {
    Ok(read_rules(directory)?
        .into_iter()
        .filter(|rule| rule.decision == Decision::Allow)
        .filter_map(|rule| {
            Some(ToolRule {
                tool: rule.tool?,
                project: rule.project,
            })
        })
        .collect())
}

/// Remove every saved `tool` rule for a server and its tools, so a later server with the same
/// name does not inherit them. Blocking.
pub fn remove_server_rules(directory: &Path, server: &str) -> Result<(), String> {
    let prefix = format!("{server}__");
    edit_rules(directory, |rules| {
        rules.retain(|rule| {
            rule.tool
                .as_deref()
                .is_none_or(|tool| tool != server && !tool.starts_with(&prefix))
        });
    })
}

/// Remove saved allow rules for exactly this MCP server or tool and project. Blocking.
pub fn remove_tool_rule(directory: &Path, rule: &ToolRule) -> Result<(), String> {
    edit_rules(directory, |rules| {
        rules.retain(|saved| {
            saved.decision != Decision::Allow
                || saved.tool.as_deref() != Some(rule.tool.as_str())
                || saved.project != rule.project
        });
    })
}

fn read_rules(directory: &Path) -> Result<Vec<Rule>, String> {
    use std::io::Read;
    let unusable = "The policy file could not be read or is not valid, so it was left unchanged. Fix or remove policy.json in the Blackwall data folder, then try again.";
    let mut body = vec![];
    match std::fs::File::open(directory.join(POLICY_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err(unusable.into()),
        Ok(file) => file
            .take(MAX_POLICY_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| unusable)?,
    };
    if body.len() > MAX_POLICY_BYTES {
        return Err(unusable.into());
    }
    serde_json::from_slice::<RuleFile>(&body)
        .ok()
        .and_then(|file| validate(file.rules).ok())
        .ok_or_else(|| unusable.into())
}

/// Serializes rule edits in this process, so two saves cannot both read the old file.
static EDITS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn edit_rules(directory: &Path, change: impl FnOnce(&mut Vec<Rule>)) -> Result<(), String> {
    let _edit = EDITS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rules = read_rules(directory)?;
    let before = rules.len();
    change(&mut rules);
    if rules.len() == before && !directory.join(POLICY_FILE).exists() {
        // Nothing to save, so a missing file stays missing.
        return Ok(());
    }
    let rules = validate(rules)?;
    let mut body = serde_json::to_vec_pretty(&RuleFile { rules })
        .map_err(|_| "The policy file could not be saved.")?;
    body.push(b'\n');
    if body.len() > MAX_POLICY_BYTES {
        return Err("The policy file is too large.".into());
    }
    let path = directory.join(POLICY_FILE);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("The policy file must not be a symbolic link.".into());
    }
    replace_owner_file(directory, &path, &body)
        .map_err(|_| "The policy file could not be saved.".into())
}

/// Write a private temporary file and rename it over `path`, so a failed write leaves the old file.
pub(crate) fn replace_owner_file(
    directory: &Path,
    path: &Path,
    body: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = directory.join(format!(".save-{:016x}", rand::random::<u64>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let result = options
        .open(&temporary)
        .and_then(|mut file| file.write_all(body).and_then(|()| file.sync_all()))
        .and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Open and exclusively lock a private owner-data lock file. The returned handle holds the
/// cross-process lock until it is dropped.
pub(crate) fn lock_owner_file(directory: &Path, name: &str) -> std::io::Result<std::fs::File> {
    let path = directory.join(name);
    // Like every other owner file, never follow a planted link to somewhere else.
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(std::io::Error::other("lock file is not a regular file"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    file.lock()?;
    Ok(file)
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

/// Redact credentials line by line, keeping line breaks.
pub(crate) fn redact_text(text: &str) -> String {
    text.lines().map(redact).collect::<Vec<_>>().join("\n")
}

fn redact(subject: &str) -> String {
    let mut hide_next = false;
    let mut quoted_header = None;
    subject
        .split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            if let Some(quote) = quoted_header {
                hide_next = false;
                if word.ends_with(quote) {
                    quoted_header = None;
                }
                return "[redacted]".to_owned();
            }
            let secret = hide_next;
            hide_next = lower.ends_with("bearer")
                || ["-u", "--user", "--password", "--token", "--api-key"].contains(&lower.as_str());
            // A header name may follow `--header=` and may open a quote: `'X-Api-Key: v`,
            // `--header='Authorization: Bearer v'`, `Authorization:'Bearer v'`.
            let header = word.split_once(':').filter(|(name, _)| {
                let name = name
                    .rsplit('=')
                    .next()
                    .unwrap_or(name)
                    .trim_start_matches(['\'', '"'])
                    .to_ascii_lowercase();
                name == "authorization"
                    || name == "proxy-authorization"
                    || name == "cookie"
                    || name == "set-cookie"
                    || name.contains("api-key")
                    || name.contains("apikey")
                    || name.contains("token")
                    || name.contains("secret")
            });
            if secret {
                "[redacted]".to_owned()
            } else if let Some((name, value)) = header {
                // A quote opened anywhere in this word and not closed by its end means the value
                // continues into the following words.
                quoted_header = word
                    .find(['\'', '"'])
                    .and_then(|at| word[at..].chars().next())
                    .filter(|quote| word.matches(*quote).count() % 2 == 1);
                let value = value.trim_matches(['\'', '"']);
                if value.is_empty() {
                    hide_next = quoted_header.is_none();
                    word.to_owned()
                } else {
                    if ["bearer", "basic", "digest"].contains(&value.to_ascii_lowercase().as_str())
                    {
                        hide_next = quoted_header.is_none();
                    }
                    format!("{name}:[redacted]")
                }
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
            directory: std::env::temp_dir(),
            workspace: PathBuf::from("/workspace"),
        }
    }

    #[tokio::test]
    async fn chat_uses_global_network_rules_and_ignores_project_rules() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-chat-policy-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let directory = tokio::fs::canonicalize(directory).await.unwrap();
        let rules = json!({"rules":[
            {"decision":"deny","kind":"network","host":"blocked.example"},
            {"decision":"allow","kind":"network","host":"allowed.example"},
            {"decision":"allow","kind":"network","host":"project.example","project":directory}
        ]});
        tokio::fs::write(directory.join(POLICY_FILE), rules.to_string())
            .await
            .unwrap();
        let policy = Policy::load_global(&directory).await;
        assert!(policy.error.is_none());
        assert_eq!(
            policy.decide(ApprovalKind::Network, "blocked.example"),
            Decision::Deny
        );
        assert_eq!(
            policy.decide(ApprovalKind::Network, "allowed.example"),
            Decision::Allow
        );
        assert_eq!(
            policy.decide(ApprovalKind::Network, "project.example"),
            Decision::Ask
        );
        tokio::fs::remove_dir_all(directory).await.unwrap();
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
        for secret in [
            "hunter2",
            "abc123",
            "xyz789",
            "pw4",
            "header-key",
            "attached-key",
            "cookie-value",
            "theme-value",
            "custom-value",
            "wget-key",
            "httpie-key",
        ] {
            assert!(!audit.contains(secret), "{secret}");
        }
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn tool_rules_match_only_mcp_calls_and_their_project() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-tools-{}", rand::random::<u64>()));
        let project = directory.join("project");
        tokio::fs::create_dir_all(&project).await.unwrap();
        let project = tokio::fs::canonicalize(&project).await.unwrap();
        tokio::fs::write(
            directory.join(POLICY_FILE),
            serde_json::json!({"rules":[
                {"decision":"allow","kind":"exec","tool":"docs"},
                {"decision":"deny","kind":"exec","tool":"docs__delete"},
                {"decision":"allow","kind":"exec","tool":"files__read","project":project},
                {"decision":"allow","kind":"exec","command":"jira__search"}
            ]})
            .to_string(),
        )
        .await
        .unwrap();
        let here = Policy::load(&directory, &project).await;
        assert!(here.error.is_none());
        assert_eq!(here.decide_tool("docs__search"), Decision::Allow);
        assert_eq!(here.decide_tool("docs__delete"), Decision::Deny);
        assert_eq!(here.decide_tool("files__read"), Decision::Allow);
        assert_eq!(here.decide_tool("files__write"), Decision::Ask);
        assert_eq!(here.decide_tool("jira__search"), Decision::Allow);
        // A shell command spelled like a server or tool never matches a tool rule.
        assert_eq!(here.decide(ApprovalKind::Exec, "docs"), Decision::Ask);
        assert_eq!(
            here.decide(ApprovalKind::Exec, "files__read"),
            Decision::Ask
        );
        let elsewhere = Policy::load(&directory, Path::new("/workspace")).await;
        assert_eq!(elsewhere.decide_tool("files__read"), Decision::Ask);
        assert_eq!(elsewhere.decide_tool("docs__search"), Decision::Allow);
        for body in [
            r#"{"rules":[{"decision":"allow","kind":"exec","tool":"Bad Name"}]}"#,
            r#"{"rules":[{"decision":"allow","kind":"file","tool":"docs"}]}"#,
            r#"{"rules":[{"decision":"allow","kind":"exec","tool":"docs","command":"ls"}]}"#,
            r#"{"rules":[{"decision":"allow","kind":"exec","tool":"docs","project":"relative"}]}"#,
        ] {
            tokio::fs::write(directory.join(POLICY_FILE), body)
                .await
                .unwrap();
            assert!(
                Policy::load(&directory, &project).await.error.is_some(),
                "{body}"
            );
        }
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[test]
    fn saving_tool_rules_keeps_other_rules_and_refuses_invalid_files() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-save-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(POLICY_FILE);
        std::fs::write(
            &path,
            r#"{"rules":[{"decision":"deny","kind":"file","path":".git"}]}"#,
        )
        .unwrap();
        allow_tool(&directory, "docs", None).unwrap();
        allow_tool(&directory, "docs", None).unwrap();
        let app = directory.join("app");
        let app_path = app.to_string_lossy().into_owned();
        allow_tool(&directory, "files__read", Some(&app)).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({"rules":[
                {"decision":"deny","kind":"file","path":".git"},
                {"decision":"allow","kind":"exec","tool":"docs"},
                {"decision":"allow","kind":"exec","tool":"files__read","project":app_path}
            ]})
        );
        let read = ToolRule {
            tool: "files__read".into(),
            project: Some(app_path.clone()),
        };
        assert_eq!(
            tool_rules(&directory).unwrap(),
            vec![
                ToolRule {
                    tool: "docs".into(),
                    project: None
                },
                read.clone()
            ]
        );
        remove_tool_rule(&directory, &read).unwrap();
        assert_eq!(tool_rules(&directory).unwrap().len(), 1);
        allow_tool(&directory, "docs__fetch", None).unwrap();
        allow_tool(&directory, "docs-extra", None).unwrap();
        remove_server_rules(&directory, "docs").unwrap();
        assert_eq!(
            tool_rules(&directory).unwrap(),
            vec![ToolRule {
                tool: "docs-extra".into(),
                project: None
            }]
        );
        let empty = directory.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        remove_server_rules(&empty, "docs").unwrap();
        assert!(!empty.join(POLICY_FILE).exists());
        std::fs::write(&path, "not json").unwrap();
        let error = allow_tool(&directory, "docs", None).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn simultaneous_saves_keep_every_rule() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-race-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        std::thread::scope(|scope| {
            for index in 0..8 {
                let directory = &directory;
                scope.spawn(move || {
                    allow_tool(directory, &format!("server-{index}"), None).unwrap()
                });
            }
        });
        assert_eq!(tool_rules(&directory).unwrap().len(), 8);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    async fn record_secret(policy: &Policy) {
        policy
            .record(
                ApprovalKind::Exec,
                "curl -H Authorization: abc123 -H Authorization:Bearer xyz789 -u me:pw4 \
                 -H 'X-Api-Key: header-key' -H Authorization:attached-key \
                 -H \"Cookie: session=cookie-value; theme=theme-value\" \
                 -H X-Custom-Token:custom-value --header='X-Api-Key: wget-key' \
                 Authorization:'Bearer httpie-key' \
                 https://me:hunter2@host API_TOKEN=hunter2",
                Decision::Ask,
                false,
                None,
            )
            .await;
    }
}
