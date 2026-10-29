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

/// One allow, ask, or deny rule in the owner's `policy.json` or a project's `project.json`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub(crate) decision: Decision,
    pub(crate) kind: ApprovalKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<String>,
    /// An MCP server name, matching all of its tools, or one `server__tool` name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool: Option<String>,
    /// Limits the rule to runs in this project folder. Owner rules only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project: Option<String>,
}

/// How the rules inside one file rank against each other. Across files, a deny or ask from any
/// file always beats an allow from any other file.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleMode {
    /// Deny outranks ask, and ask outranks allow: today's behaviour.
    #[default]
    Strictest,
    /// The first matching rule in the file decides that file's answer, so an earlier allow can
    /// make an exception to a later ask or deny in the same file.
    Ordered,
}

/// Which file a rule came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    Project,
}

/// The rule that decided an action, as recorded in the audit log.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MatchedRule {
    pub source: Source,
    /// The rule's position in its file's `rules` list, from 0.
    pub index: usize,
    pub mode: RuleMode,
}

/// A decision and the rule that made it; `matched` is `None` for the default Ask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verdict {
    pub decision: Decision,
    pub matched: Option<MatchedRule>,
}

/// The contents of a rules file: `policy.json`, or the `policy` part of a project file.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RulesFile {
    #[serde(default, skip_serializing_if = "is_strictest")]
    pub mode: RuleMode,
    pub rules: Vec<Rule>,
    /// Each kept rule's position in the file as written, when some rules were dropped.
    #[serde(skip)]
    positions: Option<Vec<usize>>,
}

fn is_strictest(mode: &RuleMode) -> bool {
    *mode == RuleMode::Strictest
}

impl RulesFile {
    pub(crate) fn validate(self) -> Result<Self, &'static str> {
        let mode = self.mode;
        validate(self.rules).map(|rules| Self {
            mode,
            rules,
            positions: None,
        })
    }

    /// Keep only deny and ask rules, which can only tighten, remembering their positions.
    pub(crate) fn keep_restricting(&mut self) {
        let positions = self.positions();
        let (positions, rules) = std::mem::take(&mut self.rules)
            .into_iter()
            .zip(positions)
            .filter(|(rule, _)| rule.decision != Decision::Allow)
            .map(|(rule, index)| (index, rule))
            .unzip();
        self.rules = rules;
        self.positions = Some(positions);
    }

    /// Count the rules of each decision: (allow, ask, deny).
    pub fn counts(&self) -> (usize, usize, usize) {
        self.rules
            .iter()
            .fold((0, 0, 0), |(allow, ask, deny), rule| match rule.decision {
                Decision::Allow => (allow + 1, ask, deny),
                Decision::Ask => (allow, ask + 1, deny),
                Decision::Deny => (allow, ask, deny + 1),
            })
    }

    fn positions(&self) -> Vec<usize> {
        self.positions
            .clone()
            .unwrap_or_else(|| (0..self.rules.len()).collect())
    }

    fn layer(self, source: Source) -> Layer {
        let positions = self.positions();
        Layer {
            source,
            mode: self.mode,
            rules: positions.into_iter().zip(self.rules).collect(),
        }
    }
}

/// One file's rules, each with its position in that file.
#[derive(Debug)]
struct Layer {
    source: Source,
    mode: RuleMode,
    rules: Vec<(usize, Rule)>,
}

impl Layer {
    /// This file's answer for an action, or `None` when none of its rules match.
    fn verdict(&self, applies: &dyn Fn(&Rule) -> bool) -> Option<(Decision, MatchedRule)> {
        let mut matching = self.rules.iter().filter(|(_, rule)| applies(rule));
        let (index, rule) = match self.mode {
            RuleMode::Ordered => matching.next()?,
            RuleMode::Strictest => strictest(matching, |(_, rule)| rule.decision)?,
        };
        Some((
            rule.decision,
            MatchedRule {
                source: self.source,
                index: *index,
                mode: self.mode,
            },
        ))
    }
}

/// Raise an Allow to Ask. The rule no longer decided, so none is recorded.
fn floor_at_ask(verdict: Verdict) -> Verdict {
    if verdict.decision < Decision::Ask {
        Verdict {
            decision: Decision::Ask,
            matched: None,
        }
    } else {
        verdict
    }
}

/// The first item with the highest decision.
fn strictest<T>(items: impl Iterator<Item = T>, decision: impl Fn(&T) -> Decision) -> Option<T> {
    items.fold(None, |best, item| match best {
        Some(best) if decision(&best) >= decision(&item) => Some(best),
        _ => Some(item),
    })
}

/// A saved rule that allows an MCP server or tool, as listed for the owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRule {
    pub tool: String,
    pub project: Option<String>,
}

/// Rules loaded for one run: the owner's file, then the project's. A missing file has no rules;
/// an invalid owner file has no rules and an error, and an unusable project file makes every
/// action ask, so nothing loosens by accident.
#[derive(Debug)]
pub struct Policy {
    layers: Vec<Layer>,
    pub error: Option<String>,
    /// Notes about the project's settings file, shown with the run's other warnings.
    pub warnings: Vec<String>,
    /// Set when the project file could not be used: nothing is allowed without asking.
    ask_all: bool,
    audit: PathBuf,
    /// The data folder relative to the workspace, when the workspace contains it.
    protected: Option<PathBuf>,
    directory: PathBuf,
    workspace: PathBuf,
    with_project: bool,
}

impl Policy {
    /// Chat has no project scope. Only the owner's global rules may authorize web requests.
    pub async fn load_global(directory: &Path) -> Self {
        let mut policy = Self::read(directory, directory, false).await;
        for layer in &mut policy.layers {
            layer.rules.retain(|(_, rule)| rule.project.is_none());
        }
        policy
    }

    /// The owner's rules plus the project's `.blackwall/project.json` rules. An untrusted project
    /// contributes only its deny and ask rules.
    pub async fn load(directory: &Path, workspace: &Path) -> Self {
        Self::read(directory, workspace, true).await
    }

    async fn read(directory: &Path, workspace: &Path, with_project: bool) -> Self {
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RulesFile::default()),
            Err(_) => Err("The policy file could not be read."),
            Ok(body) if body.len() > MAX_POLICY_BYTES => Err("The policy file is too large."),
            Ok(body) => serde_json::from_slice::<RulesFile>(&body)
                .map_err(|_| "The policy file is not valid.")
                .and_then(RulesFile::validate),
        };
        let (file, mut error) = match parsed {
            Ok(file) => (file, None),
            Err(error) => (
                RulesFile::default(),
                Some(format!("{error} Every action will ask for approval.")),
            ),
        };
        let mut user = file.layer(Source::User);
        // Project rules apply only in their own folder. The workspace path is already canonical.
        let mut kept = Vec::with_capacity(user.rules.len());
        for (index, rule) in user.rules {
            // A project's MCP servers (`p-`) are only ever allowed within a project.
            let project_tool = rule
                .tool
                .as_deref()
                .is_some_and(|tool| tool.starts_with("p-"));
            if project_tool && rule.project.is_none() && rule.decision == Decision::Allow {
                continue;
            }
            if let Some(project) = &rule.project {
                let folder =
                    tokio::time::timeout(IO_TIMEOUT, tokio::fs::canonicalize(project)).await;
                if folder.ok().and_then(Result::ok).as_deref() != Some(workspace) {
                    continue;
                }
            }
            kept.push((index, rule));
        }
        user.rules = kept;
        let mut layers = vec![user];
        let mut warnings = vec![];
        let mut ask_all = false;
        if with_project {
            let (data, folder) = (directory.to_path_buf(), workspace.to_path_buf());
            let loaded = tokio::time::timeout(
                IO_TIMEOUT,
                tokio::task::spawn_blocking(move || crate::project::try_load(&folder, &data)),
            )
            .await;
            match loaded {
                Ok(Ok(Ok(loaded))) => {
                    if loaded.ask_all {
                        // Behave as the warning says: nothing is allowed without asking.
                        ask_all = true;
                        if let Some(message) = loaded.warnings.first() {
                            error.get_or_insert(message.clone());
                        }
                    }
                    warnings = loaded.warnings;
                    layers.extend(
                        loaded
                            .config
                            .policy
                            .map(|rules| rules.layer(Source::Project)),
                    );
                }
                Ok(Ok(Err(problem))) => {
                    ask_all = true;
                    let message = format!("{problem} Every action will ask for approval.");
                    warnings.push(message.clone());
                    error.get_or_insert(message);
                }
                _ => {
                    ask_all = true;
                    let message = "The project settings file could not be read in time. Every action will ask for approval.".to_owned();
                    warnings.push(message.clone());
                    error.get_or_insert(message);
                }
            }
        }
        Self {
            layers,
            error,
            warnings,
            ask_all,
            audit,
            protected,
            directory: directory.into(),
            workspace: workspace.into(),
            with_project,
        }
    }

    /// Reload the rules for the same data folder and workspace.
    pub async fn reload(&self) -> Self {
        Self::read(&self.directory, &self.workspace, self.with_project).await
    }

    /// The owner's data folder and the run's workspace, for saving a rule from an approval.
    pub fn scope(&self) -> (&Path, &Path) {
        (&self.directory, &self.workspace)
    }

    /// Each file gives its own answer (see [`RuleMode`]); across files deny outranks ask and ask
    /// outranks allow, and on a tie the owner's rule is the one recorded.
    fn verdict(&self, applies: &dyn Fn(&Rule) -> bool) -> Verdict {
        let verdict = strictest(
            self.layers
                .iter()
                .filter_map(|layer| layer.verdict(applies)),
            |(decision, _)| *decision,
        )
        .map_or(
            Verdict {
                decision: Decision::Ask,
                matched: None,
            },
            |(decision, matched)| Verdict {
                decision,
                matched: Some(matched),
            },
        );
        if self.ask_all {
            floor_at_ask(verdict)
        } else {
            verdict
        }
    }

    /// Unmatched actions ask, and so do writes to the data folder, so the agent cannot change
    /// its own rules.
    pub fn evaluate(&self, kind: ApprovalKind, subject: &str) -> Verdict {
        let verdict = self.verdict(&|rule| rule.kind == kind && matches(rule, subject));
        // Writes to the data folder or to the project's own `.blackwall` settings folder always ask,
        // so the agent cannot change rules or settings without the owner seeing it.
        let protected = kind == ApprovalKind::File
            && match normalize(&subject.to_lowercase()) {
                None => self.protected.is_some(),
                Some(path) => {
                    path.starts_with(crate::project::PROJECT_DIRECTORY)
                        || self
                            .protected
                            .as_ref()
                            .is_some_and(|data| path.starts_with(data))
                }
            };
        if protected {
            floor_at_ask(verdict)
        } else {
            verdict
        }
    }

    pub fn decide(&self, kind: ApprovalKind, subject: &str) -> Decision {
        self.evaluate(kind, subject).decision
    }

    /// MCP calls match `tool` rules by server or exact name, and `command` rules by the namespaced
    /// name. `tool` rules never match shell commands, which `decide` handles.
    pub fn evaluate_tool(&self, exposed: &str) -> Verdict {
        let server = exposed
            .split_once("__")
            .map_or(exposed, |(server, _)| server);
        self.verdict(&|rule| {
            rule.kind == ApprovalKind::Exec
                && match rule.tool.as_deref() {
                    Some(tool) => tool == exposed || tool == server,
                    None => matches(rule, exposed),
                }
        })
    }

    /// An owner HTTP command tool's request. Rules naming the tool always apply, and so do the
    /// host's deny and ask rules; a host allow applies only to a request without credentials or
    /// a body, so a credentialed request needs an allow rule that names the tool.
    pub fn evaluate_http_tool(&self, tool: &str, host: &str, credentialed: bool) -> Verdict {
        self.verdict(&|rule| {
            rule.kind == ApprovalKind::Network
                && match rule.tool.as_deref() {
                    Some(name) => name == tool,
                    None => {
                        (!credentialed || rule.decision != Decision::Allow) && matches(rule, host)
                    }
                }
        })
    }

    pub fn decide_tool(&self, exposed: &str) -> Decision {
        self.evaluate_tool(exposed).decision
    }

    /// Append a bounded, redacted record. Audit failures never change the decision.
    /// `saved` is the rule the owner saved from this approval, if any; `rule` is the rule that
    /// decided, or `None` for the default Ask.
    pub async fn record(
        &self,
        kind: ApprovalKind,
        subject: &str,
        decision: Decision,
        allowed: bool,
        saved: Option<&ToolRule>,
        rule: Option<MatchedRule>,
    ) {
        let _ = tokio::time::timeout(
            IO_TIMEOUT,
            self.append(kind, subject, decision, allowed, saved, rule),
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
        rule: Option<MatchedRule>,
    ) {
        let path = &self.audit;
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut entry = json!({"time":time,"kind":kind,"subject":redact(subject),"decision":decision,"allowed":allowed,"rule":rule});
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
            // A `tool` network rule names one owner HTTP command tool.
            ApprovalKind::Network => rule
                .host
                .as_deref()
                .filter(|h| !h.trim().is_empty())
                .or(rule
                    .tool
                    .as_deref()
                    .filter(|t| crate::command_tools::valid_name(t))),
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
    edit_rules(directory, |file| {
        if file.mode == RuleMode::Ordered {
            // The first match decides, so the owner's choice goes first to take effect.
            file.rules.retain(|saved| saved != &rule);
            file.rules.insert(0, rule);
        } else if !file.rules.contains(&rule) {
            file.rules.push(rule);
        }
    })
}

/// The owner's saved allow rules for MCP servers and tools. Blocking.
pub fn tool_rules(directory: &Path) -> Result<Vec<ToolRule>, String> {
    Ok(read_rules(directory)?
        .into_iter()
        .filter(|rule| rule.decision == Decision::Allow && rule.kind == ApprovalKind::Exec)
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
    edit_rules(directory, |file| {
        file.rules.retain(|rule| {
            rule.kind != ApprovalKind::Exec
                || rule
                    .tool
                    .as_deref()
                    .is_none_or(|tool| tool != server && !tool.starts_with(&prefix))
        });
    })
}

/// Remove saved allow rules for exactly this MCP server or tool and project. Blocking.
pub fn remove_tool_rule(directory: &Path, rule: &ToolRule) -> Result<(), String> {
    edit_rules(directory, |file| {
        file.rules.retain(|saved| {
            saved.decision != Decision::Allow
                || saved.kind != ApprovalKind::Exec
                || saved.tool.as_deref() != Some(rule.tool.as_str())
                || saved.project != rule.project
        });
    })
}

fn read_rules(directory: &Path) -> Result<Vec<Rule>, String> {
    read_file(directory).map(|file| file.rules)
}

fn read_file(directory: &Path) -> Result<RulesFile, String> {
    use std::io::Read;
    let unusable = "The policy file could not be read or is not valid, so it was left unchanged. Fix or remove policy.json in the Blackwall data folder, then try again.";
    let mut body = vec![];
    match std::fs::File::open(directory.join(POLICY_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RulesFile::default())
        }
        Err(_) => return Err(unusable.into()),
        Ok(file) => file
            .take(MAX_POLICY_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| unusable)?,
    };
    if body.len() > MAX_POLICY_BYTES {
        return Err(unusable.into());
    }
    serde_json::from_slice::<RulesFile>(&body)
        .ok()
        .and_then(|file| file.validate().ok())
        .ok_or_else(|| unusable.into())
}

/// Serializes rule edits in this process, so two saves cannot both read the old file.
static EDITS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn edit_rules(directory: &Path, change: impl FnOnce(&mut RulesFile)) -> Result<(), String> {
    let _edit = EDITS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut file = read_file(directory)?;
    let before = file.rules.len();
    change(&mut file);
    if file.rules.len() == before && !directory.join(POLICY_FILE).exists() {
        // Nothing to save, so a missing file stays missing.
        return Ok(());
    }
    // The owner's mode is kept as written.
    let file = file.validate()?;
    let mut body =
        serde_json::to_vec_pretty(&file).map_err(|_| "The policy file could not be saved.")?;
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
pub(crate) fn normalize(path: &str) -> Option<PathBuf> {
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
            layers: vec![serde_json::from_str::<RulesFile>(body)
                .unwrap()
                .validate()
                .unwrap()
                .layer(Source::User)],
            error: None,
            warnings: vec![],
            ask_all: false,
            with_project: false,
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

    #[test]
    fn ordered_mode_first_match_wins() {
        let rules = r#"[
            {"decision":"allow","kind":"exec","command":"git status"},
            {"decision":"ask","kind":"exec","command":"git"},
            {"decision":"allow","kind":"exec","command":"git log"},
            {"decision":"deny","kind":"exec","command":"git push"}
        ]"#;
        let ordered = policy(&format!(r#"{{"mode":"ordered","rules":{rules}}}"#));
        assert_eq!(
            ordered.decide(ApprovalKind::Exec, "git status -s"),
            Decision::Allow
        );
        // The earlier ask decides before the later allow and deny.
        assert_eq!(ordered.decide(ApprovalKind::Exec, "git log"), Decision::Ask);
        assert_eq!(
            ordered.decide(ApprovalKind::Exec, "git push"),
            Decision::Ask
        );
        assert_eq!(
            ordered.evaluate(ApprovalKind::Exec, "git status").matched,
            Some(MatchedRule {
                source: Source::User,
                index: 0,
                mode: RuleMode::Ordered
            })
        );
        // An allow still never matches control syntax, so it cannot shadow a later rule that way.
        assert_eq!(
            ordered.decide(ApprovalKind::Exec, "git status; rm -rf ."),
            Decision::Ask
        );
    }

    #[test]
    fn strictest_mode_unchanged() {
        let strictest = policy(
            r#"{"mode":"strictest","rules":[
                {"decision":"allow","kind":"exec","command":"git status"},
                {"decision":"ask","kind":"exec","command":"git"},
                {"decision":"deny","kind":"exec","command":"git push"}
            ]}"#,
        );
        assert_eq!(
            strictest.decide(ApprovalKind::Exec, "git status"),
            Decision::Ask
        );
        assert_eq!(
            strictest.decide(ApprovalKind::Exec, "git push"),
            Decision::Deny
        );
        assert_eq!(
            strictest.evaluate(ApprovalKind::Exec, "git push").matched,
            Some(MatchedRule {
                source: Source::User,
                index: 2,
                mode: RuleMode::Strictest
            })
        );
        assert_eq!(strictest.evaluate(ApprovalKind::Exec, "ls").matched, None);
    }

    #[test]
    fn legacy_policy_loads_unchanged() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-legacy-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let legacy = r#"{"rules":[{"decision":"deny","kind":"file","path":".git"},{"decision":"allow","kind":"exec","command":"cargo test"}]}"#;
        std::fs::write(directory.join(POLICY_FILE), legacy).unwrap();
        let file = read_file(&directory).unwrap();
        assert_eq!(file.mode, RuleMode::Strictest);
        assert_eq!(file.counts(), (1, 0, 1));
        // Saving keeps the file free of a mode, and an ordered owner file keeps its mode.
        allow_tool(&directory, "docs", None).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join(POLICY_FILE)).unwrap()).unwrap();
        assert!(saved.get("mode").is_none(), "{saved}");
        std::fs::write(
            directory.join(POLICY_FILE),
            r#"{"mode":"ordered","rules":[]}"#,
        )
        .unwrap();
        allow_tool(&directory, "docs", None).unwrap();
        assert_eq!(read_file(&directory).unwrap().mode, RuleMode::Ordered);
        std::fs::write(
            directory.join(POLICY_FILE),
            r#"{"mode":"loose","rules":[]}"#,
        )
        .unwrap();
        assert!(read_file(&directory).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn data_folder_writes_still_floor_at_ask() {
        for mode in ["strictest", "ordered"] {
            let rules = policy(&format!(
                r#"{{"mode":"{mode}","rules":[{{"decision":"allow","kind":"file","path":"."}}]}}"#
            ));
            let verdict = rules.evaluate(ApprovalKind::File, ".blackwall/policy.json");
            assert_eq!(verdict.decision, Decision::Ask, "{mode}");
            assert_eq!(verdict.matched, None, "{mode}");
            assert_eq!(
                rules.decide(ApprovalKind::File, "src/main.rs"),
                Decision::Allow
            );
        }
    }

    #[test]
    fn project_settings_writes_floor_at_ask() {
        let mut rules =
            policy(r#"{"mode":"ordered","rules":[{"decision":"allow","kind":"file","path":"."}]}"#);
        // Even when the data folder is elsewhere.
        rules.protected = None;
        for path in [
            ".blackwall/project.json",
            ".BlackWall/Project.json",
            "src/../.blackwall/new.json",
            ".blackwall",
        ] {
            assert_eq!(
                rules.decide(ApprovalKind::File, path),
                Decision::Ask,
                "{path}"
            );
        }
        assert_eq!(
            rules.decide(ApprovalKind::File, ".blackwallet/notes.md"),
            Decision::Allow
        );
        assert_eq!(
            rules.decide(ApprovalKind::File, "src/main.rs"),
            Decision::Allow
        );
    }

    #[test]
    fn saved_tool_rule_wins_in_ordered_mode() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-ordered-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(POLICY_FILE),
            r#"{"mode":"ordered","rules":[
                {"decision":"ask","kind":"exec","tool":"docs"},
                {"decision":"allow","kind":"exec","tool":"docs"}
            ]}"#,
        )
        .unwrap();
        allow_tool(&directory, "docs", None).unwrap();
        let file = read_file(&directory).unwrap();
        assert_eq!(file.mode, RuleMode::Ordered);
        // Moved to the front, not duplicated, so the owner's click takes effect.
        assert_eq!(file.counts(), (1, 1, 0));
        assert_eq!(file.rules[0].decision, Decision::Allow);
        let rules = Layer {
            source: Source::User,
            mode: file.mode,
            rules: file.rules.into_iter().enumerate().collect(),
        };
        let policy = Policy {
            layers: vec![rules],
            ..policy(r#"{"rules":[]}"#)
        };
        assert_eq!(policy.decide_tool("docs__search"), Decision::Allow);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn audit_records_deciding_rule() {
        let root = std::env::temp_dir().join(format!("blackwall-audit-{}", rand::random::<u64>()));
        let (data, work) = (root.join("data"), root.join("work"));
        std::fs::create_dir_all(work.join(".blackwall")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let work = std::fs::canonicalize(work).unwrap();
        std::fs::write(
            data.join(POLICY_FILE),
            json!({"rules":[
                {"decision":"ask","kind":"exec","command":"npm"},
                {"decision":"allow","kind":"exec","command":"cat"}
            ]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            work.join(".blackwall").join("project.json"),
            json!({"policy":{"rules":[
                {"decision":"allow","kind":"exec","command":"ls"},
                {"decision":"deny","kind":"exec","command":"cat secret.txt"}
            ]}})
            .to_string(),
        )
        .unwrap();
        let approvals = crate::approvals::Approvals::default();
        approvals.load_policy("run", &data, &work).await.unwrap();
        let never = |_| panic!("a matched rule must not prompt");
        for (subject, allowed) in [("cat secret.txt", false), ("cat notes.txt", true)] {
            assert_eq!(
                approvals
                    .request("run", ApprovalKind::Exec, subject, "detail".into(), &never)
                    .await
                    .unwrap(),
                allowed
            );
        }
        let policy = Policy::load(&data, &work).await;
        let unmatched = policy.evaluate(ApprovalKind::Exec, "make");
        policy
            .record(
                ApprovalKind::Exec,
                "make",
                unmatched.decision,
                false,
                None,
                unmatched.matched,
            )
            .await;
        let audit = std::fs::read_to_string(data.join(AUDIT_FILE)).unwrap();
        let records: Vec<serde_json::Value> = audit
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records[0]["rule"],
            json!({"source":"project","index":1,"mode":"strictest"})
        );
        assert_eq!(
            records[1]["rule"],
            json!({"source":"user","index":1,"mode":"strictest"})
        );
        assert_eq!(records[2]["rule"], serde_json::Value::Null);
        assert!(records[2].as_object().unwrap().contains_key("rule"));
        std::fs::remove_dir_all(&root).unwrap();
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
                None,
            )
            .await;
    }
}
