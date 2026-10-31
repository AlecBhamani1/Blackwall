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
/// How many of the newest audit records the Rules screen shows.
const AUDIT_VIEW: usize = 100;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How much of each owner command tool argument the audit keeps.
const MAX_AUDIT_ARG: usize = 1000;
/// How many bytes of owner command tool arguments one audit record keeps in all.
const MAX_AUDIT_ARGS_BYTES: usize = 8 * 1024;
/// Options whose next word is a credential.
const SECRET_OPTIONS: [&str; 5] = ["-u", "--user", "--password", "--token", "--api-key"];

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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
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
    pub(crate) fn validate(self) -> Result<Self, String> {
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
    pub(crate) project_limits: Option<crate::project::ProjectLimits>,
    pub(crate) project_instructions: Option<crate::project::InstructionSettings>,
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
            Err(_) => Err("The policy file could not be read.".into()),
            Ok(body) if body.len() > MAX_POLICY_BYTES => {
                Err("The policy file is too large.".into())
            }
            Ok(body) => serde_json::from_slice::<RulesFile>(&body)
                .map_err(|_| "The policy file is not valid.".into())
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
        let mut warnings: Vec<String> = error.iter().cloned().collect();
        let mut ask_all = false;
        let mut project_limits = None;
        let mut project_instructions = None;
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
                    warnings.extend(loaded.warnings);
                    if loaded.trust == crate::project::Trust::Trusted {
                        project_limits = loaded.config.limits;
                        project_instructions = loaded.config.instructions;
                    }
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
            project_limits,
            project_instructions,
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
        self.verdict_by_source(&|_, rule| applies(rule))
    }

    fn verdict_by_source(&self, applies: &dyn Fn(Source, &Rule) -> bool) -> Verdict {
        let verdict = strictest(
            self.layers
                .iter()
                .filter_map(|layer| layer.verdict(&|rule| applies(layer.source, rule))),
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
    /// name. `tool` rules never match shell commands, which `decide` handles. A project's allow
    /// rules only ever allow its own `p-` servers, however they are written.
    pub fn evaluate_tool(&self, exposed: &str) -> Verdict {
        let server = exposed
            .split_once("__")
            .map_or(exposed, |(server, _)| server);
        self.verdict_by_source(&|source, rule| {
            rule.kind == ApprovalKind::Exec
                && (source != Source::Project
                    || rule.decision != Decision::Allow
                    || server.starts_with("p-"))
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
        self.record_program(kind, subject, None, decision, allowed, saved, rule)
            .await;
    }

    /// [`Self::record`] that also records an owner command tool's `call`: the absolute program it
    /// runs, which `subject` (the configured command) may name only by a bare name, and what it
    /// runs with.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_program(
        &self,
        kind: ApprovalKind,
        subject: &str,
        call: Option<&ProgramCall<'_>>,
        decision: Decision,
        allowed: bool,
        saved: Option<&ToolRule>,
        rule: Option<MatchedRule>,
    ) {
        let _ = tokio::time::timeout(
            IO_TIMEOUT,
            self.append(kind, subject, call, decision, allowed, saved, rule),
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn append(
        &self,
        kind: ApprovalKind,
        subject: &str,
        call: Option<&ProgramCall<'_>>,
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
        if let Some(call) = call {
            entry["program"] = json!(call.program.to_string_lossy());
            entry["tool"] = json!(call.tool);
            entry["isolation"] = json!(call.isolation.name());
            entry["args"] = json!(redact_args(call.args));
            entry["env"] = json!(call.env);
            if let Some(digest) = &call.stdin_sha256 {
                entry["stdinSha256"] = json!(digest);
            }
            if call.overridden {
                entry["overridden"] = json!(true);
            }
        }
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

fn validate(rules: Vec<Rule>) -> Result<Vec<Rule>, String> {
    if rules.len() > MAX_RULES {
        return Err("The policy file has too many rules.".into());
    }
    for (index, rule) in rules.iter().enumerate() {
        // Paths match as prefixes, so a wildcard would never match anything.
        if let Some(path) = rule.path.as_deref().filter(|p| p.contains(['*', '?'])) {
            return Err(format!(
                "Rule rules[{index}] path {path:?} uses a wildcard. Paths are plain prefixes: use \"src\", not \"src/**\"."
            ));
        }
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
            return Err("A policy rule has missing or mismatched fields.".into());
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

/// The owner's rules and mode as saved, for the Rules screen. Blocking.
pub fn owner_rules(directory: &Path) -> Result<RulesFile, String> {
    read_file(directory)
}

/// Replace the owner's rules with `rules`, keeping the mode, if the file still holds `shown`, the
/// rules the owner was looking at. Refused like any other save: same lock, same validation.
/// Blocking.
pub fn save_owner_rules(
    directory: &Path,
    shown: &RulesFile,
    rules: Vec<Rule>,
) -> Result<(), String> {
    try_edit_rules(directory, |file| {
        if file.mode != shown.mode || file.rules != shown.rules {
            return Err("The rules changed since they were shown. Reload and try again.".into());
        }
        file.rules = rules;
        Ok(())
    })
}

/// The changes from `before` to `after` that make the rules weaker, in plain words: an allow
/// rule that is new or changed, a deny or ask rule that is removed or changed, or a new mode.
fn loosened(before: &RulesFile, after: &RulesFile) -> Vec<String> {
    let describe = |rule: &Rule| {
        let subject = [&rule.command, &rule.path, &rule.host, &rule.tool]
            .into_iter()
            .flatten()
            .next()
            .map_or("", String::as_str);
        let project = rule
            .project
            .as_deref()
            .map_or_else(String::new, |p| format!(" in {p}"));
        format!(
            "{} {subject}{project}",
            format!("{:?}", rule.kind).to_lowercase()
        )
    };
    let mut changes: Vec<String> = after
        .rules
        .iter()
        .filter(|rule| rule.decision == Decision::Allow && !before.rules.contains(rule))
        .map(|rule| format!("Allow {}", describe(rule)))
        .collect();
    changes.extend(
        before
            .rules
            .iter()
            .filter(|rule| rule.decision != Decision::Allow && !after.rules.contains(rule))
            .map(|rule| {
                let decision = format!("{:?}", rule.decision).to_lowercase();
                format!("Remove {decision} {}", describe(rule))
            }),
    );
    if before.mode != after.mode {
        changes.push(
            match after.mode {
                RuleMode::Ordered => "Change the mode to ordered: the first matching rule decides",
                RuleMode::Strictest => {
                    "Change the mode to strictest: deny outranks ask, and ask outranks allow"
                }
            }
            .into(),
        );
    }
    changes
}

/// Before a save: refuse `rules` the loader would refuse, then, if they are weaker than `shown`,
/// ask `confirm` with the changes in plain words. A refusal by the owner is "Not saved.".
pub fn confirm_loosening(
    shown: &RulesFile,
    rules: &[Rule],
    confirm: impl FnOnce(&[String]) -> bool,
) -> Result<(), String> {
    let mut after = shown.clone();
    after.rules = validate(rules.to_vec())?;
    let changes = loosened(shown, &after);
    if changes.is_empty() || confirm(&changes) {
        Ok(())
    } else {
        Err("Not saved.".into())
    }
}

/// A tested subject's verdict.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTest {
    #[serde(flatten)]
    pub verdict: Verdict,
    /// No rule matched, but a rule limited to a project would in that project.
    pub project_only: bool,
}

/// What the owner's rules decide for a command, path or host, with no project and no audit record.
pub async fn test_owner_rule(
    directory: &Path,
    kind: ApprovalKind,
    subject: &str,
) -> Result<RuleTest, String> {
    // The workspace is never a parent of the data folder, so the data folder's own floor at Ask
    // does not hide what the rules say.
    let policy = Policy::read(directory, &directory.join(POLICY_FILE), false).await;
    if let Some(error) = &policy.error {
        return Err(error.clone());
    }
    let verdict = policy.evaluate(kind, subject);
    let project_only = verdict.matched.is_none()
        && read_file(directory)?
            .rules
            .iter()
            .any(|rule| rule.project.is_some() && rule.kind == kind && matches(rule, subject));
    Ok(RuleTest {
        verdict,
        project_only,
    })
}

/// One audit record, as the Rules screen shows it.
#[derive(Debug, Deserialize, Serialize)]
pub struct AuditEntry {
    pub time: u64,
    pub kind: ApprovalKind,
    pub subject: String,
    pub decision: Decision,
    pub allowed: bool,
    pub rule: Option<MatchedRule>,
}

/// The newest audit records, newest first, from the end of the log. Damaged lines are skipped.
/// Blocking.
pub fn audit_entries(directory: &Path) -> Result<Vec<AuditEntry>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let unreadable = "The audit log could not be read.";
    let mut file = match std::fs::File::open(directory.join(AUDIT_FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err(unreadable.into()),
        Ok(file) => file,
    };
    let start = file
        .metadata()
        .map_err(|_| unreadable)?
        .len()
        .saturating_sub(MAX_AUDIT_BYTES);
    file.seek(SeekFrom::Start(start)).map_err(|_| unreadable)?;
    let mut body = vec![];
    file.take(MAX_AUDIT_BYTES)
        .read_to_end(&mut body)
        .map_err(|_| unreadable)?;
    let text = String::from_utf8_lossy(&body);
    let mut lines = text.lines();
    if start > 0 {
        // The cut may have landed inside a record.
        lines.next();
    }
    Ok(lines
        .rev()
        .filter_map(|line| serde_json::from_str(line).ok())
        .take(AUDIT_VIEW)
        .collect())
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
    try_edit_rules(directory, |file| {
        change(file);
        Ok(())
    })
}

fn try_edit_rules(
    directory: &Path,
    change: impl FnOnce(&mut RulesFile) -> Result<(), String>,
) -> Result<(), String> {
    let _edit = EDITS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut file = read_file(directory)?;
    let before = file.rules.len();
    change(&mut file)?;
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

/// An owner command tool's call, for its audit record: the absolute program, the tool, where it
/// runs, its arguments after the program, the pass-through names it gets, and the SHA-256 of
/// its standard input. Never a variable's value; arguments are redacted as a subject is.
#[derive(Clone, Debug)]
pub(crate) struct ProgramCall<'a> {
    pub program: &'a Path,
    pub tool: &'a str,
    pub isolation: crate::command_tools::Isolation,
    pub args: &'a [String],
    pub env: &'a [String],
    pub stdin_sha256: Option<String>,
    /// An allow rule matched, but the tool always asks, so the rule did not decide.
    pub overridden: bool,
}

/// Each argument [`redact`]ed, and the one after a credential option or `Bearer` hidden whole.
/// Each is cut at [`MAX_AUDIT_ARG`] characters, and once they reach [`MAX_AUDIT_ARGS_BYTES`] the
/// rest are counted in a final `…(N more args)`, so calls the log records without a prompt, such
/// as denied ones, cannot rotate older records away.
fn redact_args(args: &[String]) -> Vec<String> {
    let mut hide_next = false;
    let mut total = 0;
    let mut kept = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let lower = arg.to_ascii_lowercase();
        let shown = match hide_next {
            true => "[redacted]".to_owned(),
            false => redact(arg),
        };
        hide_next = lower.ends_with("bearer") || SECRET_OPTIONS.contains(&lower.as_str());
        let shown: String = match shown.chars().nth(MAX_AUDIT_ARG) {
            Some(_) => shown.chars().take(MAX_AUDIT_ARG).chain(['…']).collect(),
            None => shown,
        };
        total += shown.len();
        if total > MAX_AUDIT_ARGS_BYTES {
            kept.push(format!("…({} more args)", args.len() - index));
            break;
        }
        kept.push(shown);
    }
    kept
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
            hide_next = lower.ends_with("bearer") || SECRET_OPTIONS.contains(&lower.as_str());
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

    #[test]
    fn audited_arguments_hide_credentials_and_stay_short() {
        let args: Vec<String> = ["--token", "abc", "Bearer", "xyz", "key=v", "plain"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            redact_args(&args),
            [
                "--token",
                "[redacted]",
                "Bearer",
                "[redacted]",
                "key=[redacted]",
                "plain"
            ]
        );
        let long = redact_args(&["é".repeat(MAX_AUDIT_ARG + 5)]);
        assert_eq!(long[0].chars().count(), MAX_AUDIT_ARG + 1);
        assert!(long[0].ends_with('…'));
    }

    #[tokio::test]
    async fn an_owner_tool_record_stays_small_with_the_most_and_longest_arguments() {
        let mut policy = policy(r#"{"rules":[]}"#);
        policy.audit = std::env::temp_dir().join(format!(
            "blackwall-audit-cap-{}.jsonl",
            rand::random::<u64>()
        ));
        // A tool's longest command line: 64 arguments of 8,000 bytes each.
        let args: Vec<String> = (0..64)
            .map(|i| format!("{i:02}{}", "x".repeat(7998)))
            .collect();
        let call = ProgramCall {
            program: Path::new("/usr/bin/tool"),
            tool: "review",
            isolation: crate::command_tools::Isolation::Outside,
            args: &args,
            env: &[],
            stdin_sha256: None,
            overridden: false,
        };
        policy
            .record_program(
                ApprovalKind::Exec,
                "tool {a}",
                Some(&call),
                Decision::Deny,
                false,
                None,
                None,
            )
            .await;
        let line = tokio::fs::read_to_string(&policy.audit).await.unwrap();
        let _ = tokio::fs::remove_file(&policy.audit).await;
        assert!(line.len() < 10 * 1024, "{} bytes", line.len());
        let record: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(record["tool"], "review");
        assert_eq!(record["isolation"], "outside");
        assert_eq!(record["program"], "/usr/bin/tool");
        let shown = record["args"].as_array().unwrap();
        let kept = shown.len() - 1;
        assert!((1..64).contains(&kept), "{kept}");
        assert_eq!(shown[kept], format!("…({} more args)", 64 - kept));
        assert!(shown[0].as_str().unwrap().starts_with("00x"));
    }

    fn policy(body: &str) -> Policy {
        Policy {
            layers: vec![serde_json::from_str::<RulesFile>(body)
                .unwrap()
                .validate()
                .unwrap()
                .layer(Source::User)],
            error: None,
            warnings: vec![],
            project_limits: None,
            project_instructions: None,
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
    fn rule_matching_fuzz_is_panic_free_and_bounded() {
        fn next(state: &mut u64) -> u8 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state as u8
        }

        let alphabet = [
            'a', 'Z', '.', '*', '/', '\\', ' ', '\t', '\n', '$', ';', 'é', '🦀',
        ];
        let mut seed = 0x706f_6c69_6379_7275;
        for _ in 0..1024 {
            let string = |seed: &mut u64| {
                (0..usize::from(next(seed)))
                    .map(|_| alphabet[usize::from(next(seed)) % alphabet.len()])
                    .collect::<String>()
            };
            let pattern = string(&mut seed);
            let subject = string(&mut seed);
            let kind = match next(&mut seed) % 4 {
                0 => ApprovalKind::Exec,
                1 => ApprovalKind::File,
                2 => ApprovalKind::Network,
                _ => ApprovalKind::Subagent,
            };
            let rule = Rule {
                decision: match next(&mut seed) % 3 {
                    0 => Decision::Allow,
                    1 => Decision::Ask,
                    _ => Decision::Deny,
                },
                kind,
                command: matches!(kind, ApprovalKind::Exec).then(|| pattern.clone()),
                path: matches!(kind, ApprovalKind::File).then(|| pattern.clone()),
                host: matches!(kind, ApprovalKind::Network).then(|| pattern.clone()),
                tool: None,
                project: None,
            };
            assert!(std::panic::catch_unwind(|| matches(&rule, &subject)).is_ok());
        }

        let long = "*.".to_owned() + &"a".repeat(MAX_POLICY_BYTES - 2);
        let rule = Rule {
            decision: Decision::Deny,
            kind: ApprovalKind::Network,
            command: None,
            path: None,
            host: Some(long),
            tool: None,
            project: None,
        };
        assert!(!matches(&rule, &"b".repeat(MAX_POLICY_BYTES)));
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

    #[test]
    fn wildcard_rule_paths_are_refused_with_the_rule_named() {
        for path in ["src/**", "*.env", "file?.txt"] {
            let rules = json!({"rules":[
                {"decision":"allow","kind":"exec","command":"ls"},
                {"decision":"deny","kind":"file","path":path}
            ]});
            let error = serde_json::from_value::<RulesFile>(rules)
                .unwrap()
                .validate()
                .unwrap_err();
            assert!(
                error.contains("rules[1]") && error.contains(path) && error.contains("plain"),
                "{error}"
            );
        }
        // Brackets are literal, as in a Next.js route folder.
        let literal = json!({"rules":[{"decision":"deny","kind":"file","path":"app/[id]"}]});
        assert!(serde_json::from_value::<RulesFile>(literal)
            .unwrap()
            .validate()
            .is_ok());
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
            assert_eq!(
                invalid.warnings,
                invalid.error.iter().cloned().collect::<Vec<_>>()
            );
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

    fn owner_directory() -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("blackwall-owner-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn rule(body: serde_json::Value) -> Rule {
        serde_json::from_value(body).unwrap()
    }

    fn shown(mode: RuleMode, rules: &[Rule]) -> RulesFile {
        RulesFile {
            mode,
            rules: rules.to_vec(),
            positions: None,
        }
    }

    fn strictest(rules: &[Rule]) -> RulesFile {
        shown(RuleMode::Strictest, rules)
    }

    fn ordered(rules: &[Rule]) -> RulesFile {
        shown(RuleMode::Ordered, rules)
    }

    #[test]
    fn a_mode_change_since_it_was_shown_is_refused() {
        let directory = owner_directory();
        let path = directory.join(POLICY_FILE);
        let kept = rule(json!({"decision":"deny","kind":"file","path":".git"}));
        std::fs::write(
            &path,
            r#"{"mode":"ordered","rules":[{"decision":"deny","kind":"file","path":".git"}]}"#,
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = save_owner_rules(
            &directory,
            &strictest(std::slice::from_ref(&kept)),
            vec![kept.clone()],
        )
        .unwrap_err();
        assert!(error.contains("changed"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn only_weaker_rules_are_loosened() {
        let deny = rule(json!({"decision":"deny","kind":"file","path":".git"}));
        let ask = rule(json!({"decision":"ask","kind":"exec","command":"git push"}));
        let allow = rule(json!({"decision":"allow","kind":"exec","command":"git status"}));
        let scoped =
            rule(json!({"decision":"allow","kind":"exec","tool":"docs","project":"/work/app"}));
        let before = strictest(&[deny.clone(), ask.clone(), allow.clone()]);
        // Equal, tightened, or an allow removed: nothing to confirm.
        let stricter = rule(json!({"decision":"deny","kind":"exec","command":"rm"}));
        for after in [
            before.clone(),
            strictest(&[deny.clone(), ask.clone(), allow.clone(), stricter]),
            strictest(&[deny.clone(), ask.clone()]),
        ] {
            assert!(loosened(&before, &after).is_empty());
        }
        assert_eq!(
            loosened(
                &before,
                &strictest(&[deny.clone(), ask.clone(), allow.clone(), scoped])
            ),
            ["Allow exec docs in /work/app"]
        );
        assert_eq!(
            loosened(&before, &strictest(&[ask.clone(), allow.clone()])),
            ["Remove deny file .git"]
        );
        // A rule changed from ask to allow is both a new allow and a removed ask.
        let changed = rule(json!({"decision":"allow","kind":"exec","command":"git push"}));
        assert_eq!(
            loosened(&before, &strictest(&[deny, changed, allow])),
            ["Allow exec git push", "Remove ask exec git push"]
        );
        assert_eq!(
            loosened(&before, &ordered(&before.rules)),
            ["Change the mode to ordered: the first matching rule decides"]
        );
        assert_eq!(
            loosened(&ordered(&before.rules), &before),
            ["Change the mode to strictest: deny outranks ask, and ask outranks allow"]
        );
    }

    #[test]
    fn loosening_asks_only_for_valid_rules_and_only_when_weaker() {
        let deny = rule(json!({"decision":"deny","kind":"file","path":".git"}));
        let allow = rule(json!({"decision":"allow","kind":"file","path":"src"}));
        let wildcard = rule(json!({"decision":"allow","kind":"file","path":"src/**"}));
        let before = strictest(std::slice::from_ref(&deny));
        let asked = std::cell::Cell::new(0);
        let confirm = |answer: bool| {
            let asked = &asked;
            move |changes: &[String]| {
                asked.set(asked.get() + 1);
                assert_eq!(changes, ["Allow file src"]);
                answer
            }
        };
        let error =
            confirm_loosening(&before, &[deny.clone(), wildcard], confirm(true)).unwrap_err();
        assert!(error.contains("wildcard"), "{error}");
        assert_eq!(asked.get(), 0);
        let both = [deny.clone(), allow];
        assert_eq!(
            confirm_loosening(&before, &both, confirm(false)).unwrap_err(),
            "Not saved."
        );
        assert_eq!(asked.get(), 1);
        confirm_loosening(&before, &both, confirm(true)).unwrap();
        // Equal and tighter rules are not asked about.
        let stricter = rule(json!({"decision":"deny","kind":"exec","command":"rm"}));
        confirm_loosening(&before, std::slice::from_ref(&deny), |_| {
            panic!("not weaker")
        })
        .unwrap();
        confirm_loosening(&before, &[deny, stricter], |_| panic!("not weaker")).unwrap();
    }

    #[test]
    fn saving_owner_rules_keeps_the_mode_and_tool_rules_and_refuses_bad_rules() {
        let directory = owner_directory();
        let path = directory.join(POLICY_FILE);
        std::fs::write(
            &path,
            r#"{"mode":"ordered","rules":[{"decision":"allow","kind":"exec","tool":"docs"}]}"#,
        )
        .unwrap();
        let shown = owner_rules(&directory).unwrap();
        assert_eq!(shown.mode, RuleMode::Ordered);
        let mut rules = shown.rules.clone();
        rules.push(rule(json!({"decision":"deny","kind":"file","path":".git"})));
        save_owner_rules(&directory, &shown, rules.clone()).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            json!({"mode":"ordered","rules":[
                {"decision":"allow","kind":"exec","tool":"docs"},
                {"decision":"deny","kind":"file","path":".git"}
            ]})
        );
        // The loader's own refusals, with its text, leave the file as it was.
        let before = std::fs::read(&path).unwrap();
        let mut wildcard = rules.clone();
        wildcard.push(rule(
            json!({"decision":"allow","kind":"file","path":"src/**"}),
        ));
        let error = save_owner_rules(&directory, &ordered(&rules), wildcard).unwrap_err();
        assert!(
            error.contains("rules[2]") && error.contains("wildcard"),
            "{error}"
        );
        let mut mismatched = rules.clone();
        mismatched.push(rule(
            json!({"decision":"allow","kind":"file","host":"example.com"}),
        ));
        assert_eq!(
            save_owner_rules(&directory, &ordered(&rules), mismatched).unwrap_err(),
            "A policy rule has missing or mismatched fields."
        );
        // A file that changed since it was shown is not overwritten.
        let error = save_owner_rules(&directory, &shown, vec![]).unwrap_err();
        assert!(error.contains("changed"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn saving_owner_rules_creates_a_missing_file_and_leaves_a_broken_one() {
        let directory = owner_directory();
        let path = directory.join(POLICY_FILE);
        assert!(owner_rules(&directory).unwrap().rules.is_empty());
        save_owner_rules(&directory, &strictest(&[]), vec![]).unwrap();
        assert!(!path.exists());
        let added = rule(json!({"decision":"ask","kind":"network","host":"example.com"}));
        save_owner_rules(&directory, &strictest(&[]), vec![added.clone()]).unwrap();
        assert_eq!(owner_rules(&directory).unwrap().rules, vec![added.clone()]);
        std::fs::write(&path, "not json").unwrap();
        let error = owner_rules(&directory).unwrap_err();
        assert!(error.contains("left unchanged"), "{error}");
        assert!(save_owner_rules(&directory, &strictest(&[]), vec![added]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saving_owner_rules_refuses_a_symbolic_link() {
        let directory = owner_directory();
        let target = directory.join("elsewhere.json");
        std::fs::write(&target, r#"{"rules":[]}"#).unwrap();
        std::os::unix::fs::symlink(&target, directory.join(POLICY_FILE)).unwrap();
        let added = rule(json!({"decision":"deny","kind":"exec","command":"rm"}));
        let error = save_owner_rules(&directory, &strictest(&[]), vec![added]).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), r#"{"rules":[]}"#);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn simultaneous_owner_rule_saves_never_lose_a_rule() {
        let directory = owner_directory();
        std::thread::scope(|scope| {
            for index in 0..8 {
                let directory = &directory;
                scope.spawn(move || loop {
                    let mut rules = owner_rules(directory).unwrap().rules;
                    let shown = strictest(&rules);
                    rules.push(rule(
                        json!({"decision":"ask","kind":"exec","command":format!("tool-{index}")}),
                    ));
                    if save_owner_rules(directory, &shown, rules).is_ok() {
                        break;
                    }
                });
            }
        });
        assert_eq!(owner_rules(&directory).unwrap().rules.len(), 8);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn testing_an_owner_rule_names_the_deciding_rule_and_records_nothing() {
        let directory = owner_directory();
        std::fs::write(
            directory.join(POLICY_FILE),
            json!({"rules":[
                {"decision":"ask","kind":"exec","command":"git"},
                {"decision":"allow","kind":"exec","command":"git status"},
                {"decision":"allow","kind":"file","path":"src"},
                {"decision":"allow","kind":"network","host":"example.com"},
                {"decision":"deny","kind":"exec","command":"rm","project":directory.to_string_lossy()}
            ]})
            .to_string(),
        )
        .unwrap();
        let matched = |index| {
            Some(MatchedRule {
                source: Source::User,
                index,
                mode: RuleMode::Strictest,
            })
        };
        let cases = [
            (ApprovalKind::Exec, "git status", Decision::Ask, matched(0)),
            (
                ApprovalKind::File,
                "src/main.rs",
                Decision::Allow,
                matched(2),
            ),
            (
                ApprovalKind::Network,
                "example.com",
                Decision::Allow,
                matched(3),
            ),
            (ApprovalKind::Exec, "make", Decision::Ask, None),
            // A rule limited to a project does not apply outside one.
            (ApprovalKind::Exec, "rm -rf x", Decision::Ask, None),
        ];
        for (kind, subject, decision, rule) in cases {
            let tested = test_owner_rule(&directory, kind, subject).await.unwrap();
            let verdict = tested.verdict;
            assert_eq!(
                (verdict.decision, verdict.matched),
                (decision, rule),
                "{subject}"
            );
            assert_eq!(tested.project_only, subject == "rm -rf x", "{subject}");
        }
        assert!(!directory.join(AUDIT_FILE).exists());
        std::fs::write(directory.join(POLICY_FILE), "not json").unwrap();
        assert!(test_owner_rule(&directory, ApprovalKind::Exec, "ls")
            .await
            .is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn the_audit_view_is_newest_first_bounded_and_skips_damaged_lines() {
        let directory = owner_directory();
        assert!(audit_entries(&directory).unwrap().is_empty());
        let mut log = String::new();
        for time in 0..150 {
            log += &json!({"time":time,"kind":"exec","subject":format!("cmd {time}"),"decision":"deny","allowed":false,"rule":{"source":"user","index":3,"mode":"strictest"},"args":["secret"]}).to_string();
            log += "\n";
            if time == 100 {
                log += "not json\n";
            }
        }
        std::fs::write(directory.join(AUDIT_FILE), log).unwrap();
        let entries = audit_entries(&directory).unwrap();
        assert_eq!(entries.len(), AUDIT_VIEW);
        assert_eq!(entries[0].time, 149);
        assert_eq!(entries[0].subject, "cmd 149");
        assert_eq!(entries[99].time, 50);
        assert_eq!(
            entries[0].rule,
            Some(MatchedRule {
                source: Source::User,
                index: 3,
                mode: RuleMode::Strictest
            })
        );
        assert!(!serde_json::to_string(&entries).unwrap().contains("secret"));
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
