//! A project's own settings in `<workspace>/.blackwall/project.json`, and the owner's trust in
//! them.
//!
//! A cloned repository can ship this file, so it is never a source of new permissions on its own:
//! - Until the owner trusts the file's exact bytes, only its `deny` and `ask` rules apply, because
//!   they can only tighten. Everything else waits for trust.
//! - Trust is recorded in the owner's `trusted-projects.json` as the canonical project folder and
//!   the SHA-256 of the file. Any edit to the file makes it untrusted again.
//! - The file is read once (a regular file, never a symbolic link, at most 64 KiB), and both the
//!   hash and the parsed settings come from those same bytes.
//!
//! The `mcp`, `limits` and `instructions` sections are parsed and validated now so the file format
//! is fixed, but nothing applies them yet. Whoever consumes them must treat the owner's own value
//! as a ceiling: a project can only lower limits, narrow tools, and lower instruction caps.
use crate::policy::{Decision, RuleMode, RulesFile};
use crate::protocol::ApprovalKind;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const PROJECT_DIRECTORY: &str = ".blackwall";
pub const PROJECT_FILE: &str = "project.json";
pub const TRUST_FILE: &str = "trusted-projects.json";
const TRUST_LOCK: &str = ".trusted-projects.lock";
pub const MAX_PROJECT_BYTES: usize = 64 * 1024;
const MAX_TRUSTED: usize = 100;
const MAX_TRUST_BYTES: usize = 16 * 1024 * 1024;
const MAX_SERVERS: usize = 16;
const MAX_EXTRA_FILES: usize = 8;
const MAX_MESSAGE_CHARS: usize = 200;
/// A diff longer than this is not shown; the whole file is shown instead.
const MAX_DIFF_LINES: usize = 5000;
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);
pub const DIFF_TOO_LARGE: &str =
    "The change is too large to show as a diff. Review the whole file below.";
/// The built-in tools a subagent can use today. A project can only narrow this list.
const SUBAGENT_TOOLS: [&str; 2] = ["read_file", "list_files"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProjectError {
    #[error("This project has no .blackwall/project.json file.")]
    Missing,
    #[error("The project settings file is larger than 64 KiB.")]
    TooLarge,
    #[error(
        "The project settings file and its .blackwall folder must be regular, not symbolic links."
    )]
    NotRegular,
    #[error("The project settings file could not be read.")]
    Unreadable,
    #[error("The project settings file is not valid: {0}")]
    Invalid(String),
    #[error("The project settings file changed after it was shown. Review it again.")]
    Changed,
    #[error("The trusted projects list could not be read or saved.")]
    TrustFile,
    #[error("Too many projects are trusted. Untrust one first.")]
    TooMany,
    #[error("The trusted projects list is too large. Untrust some projects first.")]
    TrustListTooLarge,
}

impl ProjectError {
    /// A stable name for traces and callers, without file content.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::TooLarge => "too_large",
            Self::NotRegular => "not_regular",
            Self::Unreadable => "unreadable",
            Self::Invalid(_) => "invalid",
            Self::Changed => "changed",
            Self::TrustFile => "trust_file",
            Self::TooMany => "too_many",
            Self::TrustListTooLarge => "trust_list_too_large",
        }
    }
}

fn invalid(message: impl std::fmt::Display) -> ProjectError {
    ProjectError::Invalid(
        message
            .to_string()
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .collect(),
    )
}

/// The contents of `.blackwall/project.json`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Allow, ask and deny rules, in the same format as the owner's `policy.json`, except that
    /// rules cannot name a `project`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<RulesFile>,
    /// Not applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpFile>,
    /// Not applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ProjectLimits>,
    /// Not applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<InstructionSettings>,
}

/// Project MCP servers. A project may only name servers by URL: it can never launch a command
/// or use the owner's secrets or headers, so those fields are rejected.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpFile {
    pub servers: Vec<ProjectServer>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectServer {
    pub name: String,
    /// `https`, or `http` only for `127.0.0.1`, `::1` or `localhost`. No user name or password.
    pub url: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
}

fn enabled() -> bool {
    true
}

/// Limits a project can lower. The owner's value is always the ceiling.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectLimits {
    /// 4096 to 1048576 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<ProjectSubagents>,
}

/// Subagent settings a project can narrow. It can never choose where subagents run.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSubagents {
    /// 1 to 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u8>,
    /// A subset of the read-only built-in subagent tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

impl ProjectLimits {
    /// The output cap for a run: the project's value, but never above the owner's.
    pub fn output_cap(&self, owner: u32) -> u32 {
        self.max_output_bytes.map_or(owner, |cap| cap.min(owner))
    }

    /// Subagent steps: the project's value, but never above the owner's.
    pub fn subagent_steps(&self, owner: u8) -> u8 {
        self.subagents
            .as_ref()
            .and_then(|subagents| subagents.max_steps)
            .map_or(owner, |steps| steps.min(owner))
    }

    /// The owner's subagent tools that the project also lists; a project never adds one.
    pub fn subagent_tools(&self, owner: &[String]) -> Vec<String> {
        match self.subagents.as_ref().and_then(|s| s.tools.as_ref()) {
            Some(tools) => owner
                .iter()
                .filter(|tool| tools.contains(tool))
                .cloned()
                .collect(),
            None => owner.to_vec(),
        }
    }
}

/// Instruction caps a project can lower, and extra guidance files inside the project.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionSettings {
    /// 1 to 64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_kib: Option<u16>,
    /// 8 to 256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_kib: Option<u16>,
    /// Up to 8 relative paths inside the project, with no `..`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_files: Option<Vec<String>>,
}

/// Whether the owner trusts the project's current settings file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Trust {
    /// Never trusted, or there is no file.
    None,
    Trusted,
    /// Trusted before, but the file has changed since. `diff` runs from the trusted file to the
    /// current one.
    Changed {
        diff: String,
    },
}

/// A trusted project, as listed for the owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedProject {
    /// The canonical folder, as [`untrust`] takes it.
    pub path: String,
    /// The folder for display, without a Windows `\\?\` prefix.
    pub label: String,
    pub sha256: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustRecord {
    /// The canonical project folder.
    path: String,
    sha256: String,
    /// The trusted file, so a later change can be shown as a diff.
    content: String,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustList {
    projects: Vec<TrustRecord>,
}

/// One project rule as shown for review. `subject` has invisible characters escaped.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuleView {
    pub decision: Decision,
    pub kind: ApprovalKind,
    pub subject: String,
    /// Warnings about how much the rule covers: "matches every file", "any host",
    /// "any subdomain", or "your MCP server".
    pub badges: Vec<String>,
}

/// What the owner sees before deciding: the file, its trust, and what applies now.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStatus {
    /// The canonical project folder, as trust records name it.
    pub project: String,
    /// The settings file's path, for display.
    pub path: String,
    pub exists: bool,
    pub trust: Trust,
    /// The hash of the file shown in `content`; pass it to [`trust`].
    pub sha256: Option<String>,
    /// The file for display: invisible characters escaped and long blank runs collapsed.
    pub content: Option<String>,
    /// The file's rules, parsed, in file order.
    pub rules: Vec<RuleView>,
    pub mode: RuleMode,
    /// What applies to runs now.
    pub applies: Vec<String>,
    /// What waits until the owner trusts this file.
    pub waiting: Vec<String>,
    /// Why the file cannot be used, when it cannot. Every action then asks.
    pub error: Option<String>,
}

/// Load the project's settings for a run. When the file is not trusted, the config holds only its
/// deny and ask rules. A file that cannot be used comes back empty with a warning; the run's
/// policy treats that case strictly (see [`crate::policy::Policy::load`]).
pub fn load(workspace: &Path, data_dir: &Path) -> (ProjectConfig, Trust, Vec<String>) {
    match try_load(workspace, data_dir) {
        Ok(loaded) => (loaded.config, loaded.trust, loaded.warnings),
        Err(error) => (
            ProjectConfig::default(),
            Trust::None,
            vec![error.to_string()],
        ),
    }
}

/// A run's project settings. `ask_all` asks the policy to make every action ask, because trust
/// could not be checked. A `Changed` trust carries no diff here; only [`status`] computes one.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub config: ProjectConfig,
    pub trust: Trust,
    pub warnings: Vec<String>,
    pub ask_all: bool,
}

pub(crate) fn try_load(workspace: &Path, data_dir: &Path) -> Result<Loaded, ProjectError> {
    let none = || Loaded {
        config: ProjectConfig::default(),
        trust: Trust::None,
        warnings: vec![],
        ask_all: false,
    };
    let Some(workspace) = canonical(workspace) else {
        return Ok(none());
    };
    let Some(file) = read(&workspace)? else {
        return Ok(none());
    };
    let mut warnings = vec![];
    let mut ask_all = false;
    let trust = trust_state(&workspace, data_dir, &file, false).unwrap_or_else(|_| {
        // The list is written only by Blackwall; an unreadable one may have been tampered with.
        ask_all = true;
        warnings.push("The trusted projects list could not be read, so it may have been changed outside Blackwall. Every action will ask for approval until it is fixed or removed.".into());
        Trust::None
    });
    if trust == Trust::Trusted {
        return Ok(Loaded {
            config: file.config,
            trust,
            warnings,
            ask_all,
        });
    }
    let (_, waiting) = summary(&file.config, false);
    let config = restricted(file.config);
    let mut parts = vec![];
    if let Some(line) = restricting_line(&config) {
        parts.push(format!("{line}."));
    }
    match &trust {
        Trust::Changed { .. } => parts.push(
            "This project's .blackwall/project.json changed after you trusted it, so only its deny and ask rules apply. Review the change and trust it again to apply the rest.".into(),
        ),
        _ if !waiting.is_empty() => parts.push(format!(
            "This project's .blackwall/project.json is not trusted, so only its deny and ask rules apply. Waiting for trust: {}. Review it with bw project status or in the desktop app.",
            waiting.join("; ")
        )),
        _ => {}
    }
    if !parts.is_empty() {
        warnings.push(parts.join(" "));
    }
    Ok(Loaded {
        config,
        trust,
        warnings,
        ask_all,
    })
}

/// "N deny/ask rules from this project's settings apply", when there are any.
fn restricting_line(config: &ProjectConfig) -> Option<String> {
    let (_, ask, deny) = config.policy.as_ref()?.counts();
    let count = ask + deny;
    (count > 0).then(|| {
        format!(
            "{count} deny/ask {} from this project's settings {}",
            if count == 1 { "rule" } else { "rules" },
            if count == 1 { "applies" } else { "apply" }
        )
    })
}

/// Only what can tighten: deny and ask rules.
fn restricted(config: ProjectConfig) -> ProjectConfig {
    ProjectConfig {
        policy: config.policy.map(|mut rules| {
            rules.keep_restricting();
            rules
        }),
        ..ProjectConfig::default()
    }
}

/// The file, its trust, and what applies now, for the owner to review.
pub fn status(workspace: &Path, data_dir: &Path) -> ProjectStatus {
    let folder = canonical(workspace).unwrap_or_else(|| workspace.to_path_buf());
    let mut status = ProjectStatus {
        project: folder.to_string_lossy().into_owned(),
        path: display_path(
            &folder
                .join(PROJECT_DIRECTORY)
                .join(PROJECT_FILE)
                .to_string_lossy(),
        ),
        exists: false,
        trust: Trust::None,
        sha256: None,
        content: None,
        rules: vec![],
        mode: RuleMode::Strictest,
        applies: vec![],
        waiting: vec![],
        error: None,
    };
    let file = match read(&folder) {
        Ok(Some(file)) => file,
        Ok(None) => return status,
        Err(error) => {
            status.exists = error != ProjectError::Missing;
            status.error = Some(format!("{error} Every action will ask for approval."));
            return status;
        }
    };
    status.exists = true;
    status.sha256 = Some(file.sha256.clone());
    status.content = Some(display_text(&String::from_utf8_lossy(&file.bytes)));
    if let Some(rules) = &file.config.policy {
        status.mode = rules.mode;
        status.rules = rules.rules.iter().map(view).collect();
    }
    match trust_state(&folder, data_dir, &file, true) {
        Ok(trust) => status.trust = trust,
        Err(error) => status.error = Some(error.to_string()),
    }
    (status.applies, status.waiting) = summary(&file.config, status.trust == Trust::Trusted);
    status
}

/// Trust the project's settings file, but only if it still has the hash the owner reviewed.
pub fn trust(workspace: &Path, data_dir: &Path, sha256: &str) -> Result<(), ProjectError> {
    let folder = canonical(workspace).ok_or(ProjectError::Missing)?;
    let file = read(&folder)?.ok_or(ProjectError::Missing)?;
    if file.sha256 != sha256 {
        return Err(ProjectError::Changed);
    }
    let content = String::from_utf8(file.bytes).map_err(|_| invalid("not UTF-8 text"))?;
    let path = folder.to_string_lossy().into_owned();
    edit_trust(data_dir, |list| {
        list.projects.retain(|record| record.path != path);
        if list.projects.len() >= MAX_TRUSTED {
            return Err(ProjectError::TooMany);
        }
        list.projects.push(TrustRecord {
            path,
            sha256: file.sha256,
            content,
        });
        Ok(())
    })
}

/// Stop trusting a project, named as [`trusted`] or [`ProjectStatus::project`] lists it. Returns
/// whether it was trusted.
pub fn untrust(data_dir: &Path, project: &str) -> Result<bool, ProjectError> {
    let mut removed = false;
    edit_trust(data_dir, |list| {
        let before = list.projects.len();
        list.projects.retain(|record| record.path != project);
        removed = list.projects.len() != before;
        Ok(())
    })?;
    Ok(removed)
}

/// The owner's trusted projects.
pub fn trusted(data_dir: &Path) -> Result<Vec<TrustedProject>, ProjectError> {
    Ok(read_trust(data_dir)?
        .projects
        .into_iter()
        .map(|record| TrustedProject {
            label: display_path(&record.path),
            path: record.path,
            sha256: record.sha256,
        })
        .collect())
}

/// Plain-language lines: what applies now and what waits for trust.
fn summary(config: &ProjectConfig, trusted: bool) -> (Vec<String>, Vec<String>) {
    let mut applies = vec![];
    let mut waiting = vec![];
    if let Some(rules) = &config.policy {
        let (allow, ask, deny) = rules.counts();
        if ask + deny > 0 {
            let count = ask + deny;
            applies.push(format!(
                "{count} deny/ask {} from this project's settings {}",
                if count == 1 { "rule" } else { "rules" },
                if count == 1 { "applies" } else { "apply" }
            ));
        }
        if allow > 0 {
            let line = format!(
                "{allow} allow {}",
                if allow == 1 { "rule" } else { "rules" }
            );
            if trusted {
                applies.push(line);
            } else {
                waiting.push(line);
            }
        }
    }
    let later = " (accepted; not used by this version yet)";
    let mut other = vec![];
    if let Some(mcp) = &config.mcp {
        other.push(format!("{} MCP servers{later}", mcp.servers.len()));
    }
    if config.limits.is_some() {
        other.push(format!("limits{later}"));
    }
    if config.instructions.is_some() {
        other.push(format!("instruction settings{later}"));
    }
    if trusted {
        applies.extend(other);
    } else {
        waiting.extend(other);
    }
    (applies, waiting)
}

fn canonical(workspace: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(workspace).ok()
}

/// The settings file, read once: its bytes, their hash, and the settings parsed from them.
struct ProjectFile {
    bytes: Vec<u8>,
    sha256: String,
    config: ProjectConfig,
}

fn read(workspace: &Path) -> Result<Option<ProjectFile>, ProjectError> {
    let Some(bytes) = read_bytes(workspace)? else {
        return Ok(None);
    };
    let sha256 = hash(&bytes);
    let config = parse(&bytes)?;
    Ok(Some(ProjectFile {
        bytes,
        sha256,
        config,
    }))
}

/// Read the file through the project folder's capability, refusing symbolic links for both the
/// `.blackwall` folder and the file, and anything but a regular file.
fn read_bytes(workspace: &Path) -> Result<Option<Vec<u8>>, ProjectError> {
    let directory = match Dir::open_ambient_dir(workspace, cap_std::ambient_authority()) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProjectError::Unreadable),
    };
    match directory.symlink_metadata(PROJECT_DIRECTORY) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProjectError::Unreadable),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ProjectError::NotRegular)
        }
        Ok(_) => {}
    }
    let relative = Path::new(PROJECT_DIRECTORY).join(PROJECT_FILE);
    match directory.symlink_metadata(&relative) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProjectError::Unreadable),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ProjectError::NotRegular)
        }
        Ok(_) => {}
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = directory.open_with(&relative, &options).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProjectError::Missing
        } else {
            ProjectError::NotRegular
        }
    })?;
    // Check the opened file itself, so a swap after the check above cannot slip through.
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(ProjectError::NotRegular);
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROJECT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProjectError::Unreadable)?;
    if bytes.len() > MAX_PROJECT_BYTES {
        return Err(ProjectError::TooLarge);
    }
    Ok(Some(bytes))
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse(bytes: &[u8]) -> Result<ProjectConfig, ProjectError> {
    let mut config = serde_json::from_slice::<ProjectConfig>(bytes).map_err(invalid)?;
    if let Some(rules) = config.policy.take() {
        if rules.rules.iter().any(|rule| rule.project.is_some()) {
            return Err(invalid("project rules cannot name a project"));
        }
        for rule in rules.rules.iter().filter(|r| r.decision == Decision::Allow) {
            if rule.tool.as_deref().is_some_and(|tool| !own_server(tool)) {
                return Err(invalid(
                    "project allow rules can only name the project's own p- MCP servers",
                ));
            }
            if rule.kind == ApprovalKind::File
                && rule
                    .path
                    .as_deref()
                    .and_then(crate::policy::normalize)
                    .is_some_and(|path| path.as_os_str().is_empty())
            {
                return Err(invalid("project allow rules cannot match every file"));
            }
        }
        config.policy = Some(rules.validate().map_err(invalid)?);
    }
    if let Some(mcp) = &config.mcp {
        validate_servers(&mcp.servers)?;
    }
    if let Some(limits) = &config.limits {
        validate_limits(limits)?;
    }
    if let Some(instructions) = &config.instructions {
        validate_instructions(instructions)?;
    }
    Ok(config)
}

fn validate_servers(servers: &[ProjectServer]) -> Result<(), ProjectError> {
    if servers.len() > MAX_SERVERS {
        return Err(invalid("too many MCP servers"));
    }
    for (index, server) in servers.iter().enumerate() {
        if !crate::mcp::valid_server_name(&server.name)
            || servers[..index]
                .iter()
                .any(|other| other.name == server.name)
        {
            return Err(invalid(
                "an MCP server name is missing, invalid, or repeated",
            ));
        }
        let url = reqwest::Url::parse(&server.url).map_err(|_| invalid("an MCP URL is invalid"))?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
        if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid(
                "MCP URLs must use https (http only on this computer) and carry no credentials",
            ));
        }
        let tools = server
            .allow_tools
            .iter()
            .flatten()
            .chain(&server.deny_tools);
        if tools
            .into_iter()
            .any(|tool| !crate::mcp::valid_tool_name(tool))
        {
            return Err(invalid("an MCP tool name is invalid"));
        }
    }
    Ok(())
}

fn validate_limits(limits: &ProjectLimits) -> Result<(), ProjectError> {
    if limits
        .max_output_bytes
        .is_some_and(|bytes| !(4096..=1_048_576).contains(&bytes))
    {
        return Err(invalid("max_output_bytes must be 4096 to 1048576"));
    }
    let Some(subagents) = &limits.subagents else {
        return Ok(());
    };
    if subagents
        .max_steps
        .is_some_and(|steps| !(1..=50).contains(&steps))
    {
        return Err(invalid("subagents.max_steps must be 1 to 50"));
    }
    if let Some(tools) = &subagents.tools {
        if tools.iter().enumerate().any(|(index, tool)| {
            !SUBAGENT_TOOLS.contains(&tool.as_str()) || tools[..index].contains(tool)
        }) {
            return Err(invalid(
                "subagents.tools may only list read_file and list_files, once each",
            ));
        }
    }
    Ok(())
}

fn validate_instructions(settings: &InstructionSettings) -> Result<(), ProjectError> {
    if settings
        .max_file_kib
        .is_some_and(|kib| !(1..=64).contains(&kib))
    {
        return Err(invalid("instructions.max_file_kib must be 1 to 64"));
    }
    if settings
        .max_total_kib
        .is_some_and(|kib| !(8..=256).contains(&kib))
    {
        return Err(invalid("instructions.max_total_kib must be 8 to 256"));
    }
    let files = settings.extra_files.as_deref().unwrap_or_default();
    if files.len() > MAX_EXTRA_FILES
        || files.iter().any(|file| {
            file.is_empty()
                || file.len() > 256
                || file.contains('\\')
                || !Path::new(file)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        })
    {
        return Err(invalid(
            "instructions.extra_files must be up to 8 relative paths inside the project",
        ));
    }
    Ok(())
}

fn trust_state(
    workspace: &Path,
    data_dir: &Path,
    file: &ProjectFile,
    with_diff: bool,
) -> Result<Trust, ProjectError> {
    let path = workspace.to_string_lossy();
    let list = read_trust(data_dir)?;
    Ok(
        match list.projects.into_iter().find(|record| record.path == path) {
            None => Trust::None,
            Some(record) if record.sha256 == file.sha256 => Trust::Trusted,
            Some(record) => Trust::Changed {
                diff: if with_diff {
                    diff(&record.content, &String::from_utf8_lossy(&file.bytes))
                } else {
                    String::new()
                },
            },
        },
    )
}

fn read_trust(data_dir: &Path) -> Result<TrustList, ProjectError> {
    let path = data_dir.join(TRUST_FILE);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustList::default())
        }
        Ok(metadata) if metadata.is_file() => {}
        _ => return Err(ProjectError::TrustFile),
    }
    let mut body = vec![];
    std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_TRUST_BYTES as u64 + 1).read_to_end(&mut body))
        .map_err(|_| ProjectError::TrustFile)?;
    if body.len() > MAX_TRUST_BYTES {
        return Err(ProjectError::TrustFile);
    }
    serde_json::from_slice(&body).map_err(|_| ProjectError::TrustFile)
}

/// Change the trust list under the owner-file lock, so two saves cannot both read the old list.
fn edit_trust(
    data_dir: &Path,
    change: impl FnOnce(&mut TrustList) -> Result<(), ProjectError>,
) -> Result<(), ProjectError> {
    std::fs::create_dir_all(data_dir).map_err(|_| ProjectError::TrustFile)?;
    let _lock = crate::policy::lock_owner_file(data_dir, TRUST_LOCK)
        .map_err(|_| ProjectError::TrustFile)?;
    let mut list = read_trust(data_dir)?;
    change(&mut list)?;
    let mut body = serde_json::to_vec_pretty(&list).map_err(|_| ProjectError::TrustFile)?;
    body.push(b'\n');
    if body.len() > MAX_TRUST_BYTES {
        return Err(ProjectError::TrustListTooLarge);
    }
    crate::policy::replace_owner_file(data_dir, &data_dir.join(TRUST_FILE), &body)
        .map_err(|_| ProjectError::TrustFile)
}

/// Whether an MCP `tool` rule names a project server (`p-` prefix) rather than the owner's.
fn own_server(tool: &str) -> bool {
    tool.split_once("__")
        .map_or(tool, |(server, _)| server)
        .starts_with("p-")
}

fn view(rule: &crate::policy::Rule) -> RuleView {
    let mut badges = vec![];
    let subject = if let Some(tool) = &rule.tool {
        if !own_server(tool) {
            badges.push("your MCP server".to_owned());
        }
        format!("MCP {tool}")
    } else if let Some(path) = &rule.path {
        if crate::policy::normalize(&path.to_lowercase())
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            badges.push("matches every file".to_owned());
        }
        path.clone()
    } else if let Some(host) = &rule.host {
        if let Some(domain) = host.trim_end_matches('.').strip_prefix("*.") {
            badges.push(
                if domain.contains('.') {
                    "any subdomain"
                } else {
                    "any host"
                }
                .to_owned(),
            );
        }
        host.clone()
    } else {
        rule.command.clone().unwrap_or_default()
    };
    RuleView {
        decision: rule.decision,
        kind: rule.kind,
        subject: escape_invisible(&subject),
        badges,
    }
}

/// From the trusted file to the current one, bounded in time and length.
fn diff(old: &str, new: &str) -> String {
    let text = similar::TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(old, new)
        .unified_diff()
        .header("trusted", "current")
        .to_string();
    if text.lines().count() > MAX_DIFF_LINES {
        DIFF_TOO_LARGE.to_owned()
    } else {
        escape_invisible(&text)
    }
}

/// Unicode format characters (category Cf): bidi controls, zero-width characters, the byte order
/// mark, and others that change how text looks without being visible.
fn is_format(character: char) -> bool {
    matches!(character as u32,
        0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// Show invisible format characters as `\u{XXXX}`, so text cannot hide or reorder what it says.
pub fn escape_invisible(text: &str) -> String {
    text.chars()
        .map(|character| {
            if is_format(character) {
                format!("\\u{{{:04X}}}", character as u32)
            } else {
                character.to_string()
            }
        })
        .collect()
}

/// The file as shown for review: invisible characters escaped, and runs of more than two blank
/// lines collapsed with a note saying how many were hidden.
pub fn display_text(text: &str) -> String {
    let text = escape_invisible(text);
    let mut lines: Vec<String> = vec![];
    let mut blank = 0;
    let flush = |lines: &mut Vec<String>, blank: &mut usize| {
        if *blank > 2 {
            lines.push(format!("[{} more blank lines not shown]", *blank - 2));
        }
        *blank = 0;
    };
    for line in text.split('\n') {
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 2 {
                lines.push(line.to_owned());
            }
            continue;
        }
        flush(&mut lines, &mut blank);
        lines.push(line.to_owned());
    }
    flush(&mut lines, &mut blank);
    lines.join("\n")
}

/// A path for display, without the Windows `\\?\` verbatim prefix that canonical paths carry.
pub fn display_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::policy::{Decision, Policy, POLICY_FILE};
    use crate::protocol::ApprovalKind;
    use serde_json::json;

    /// A project folder and a separate data folder, removed on drop.
    struct Folders {
        root: PathBuf,
        workspace: PathBuf,
        data: PathBuf,
    }
    impl Folders {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("blackwall-project-{}", rand::random::<u64>()));
            std::fs::create_dir_all(root.join("work").join(PROJECT_DIRECTORY)).unwrap();
            std::fs::create_dir_all(root.join("data")).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            Self {
                workspace: root.join("work"),
                data: root.join("data"),
                root,
            }
        }
        fn file(&self) -> PathBuf {
            self.workspace.join(PROJECT_DIRECTORY).join(PROJECT_FILE)
        }
        fn write(&self, value: serde_json::Value) {
            std::fs::write(self.file(), serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        }
        fn owner_rules(&self, value: serde_json::Value) {
            std::fs::write(self.data.join(POLICY_FILE), value.to_string()).unwrap();
        }
        fn trust_current(&self) {
            let sha = status(&self.workspace, &self.data).sha256.unwrap();
            trust(&self.workspace, &self.data, &sha).unwrap();
        }
        async fn policy(&self) -> Policy {
            Policy::load(&self.data, &self.workspace).await
        }
    }
    impl Drop for Folders {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn rm_and_cat() -> serde_json::Value {
        json!({"policy":{"rules":[
            {"decision":"allow","kind":"exec","command":"rm"},
            {"decision":"deny","kind":"exec","command":"cat secret.txt"}
        ]}})
    }

    #[tokio::test]
    async fn untrusted_project_allow_is_ignored() {
        let folders = Folders::new();
        folders.write(rm_and_cat());
        let policy = folders.policy().await;
        assert!(policy.error.is_none());
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "rm -rf build"),
            Decision::Ask
        );
        assert!(
            policy.warnings[0].contains("not trusted"),
            "{:?}",
            policy.warnings
        );
        let (config, trust, _) = load(&folders.workspace, &folders.data);
        assert_eq!(trust, Trust::None);
        assert_eq!(config.policy.unwrap().counts(), (0, 0, 1));
        // Other sections wait for trust too.
        folders.write(
            json!({"mcp":{"servers":[{"name":"docs","url":"https://docs.example/mcp"}]},
            "limits":{"max_output_bytes":8192}}),
        );
        let (config, _, warnings) = load(&folders.workspace, &folders.data);
        assert_eq!(config, ProjectConfig::default());
        assert!(warnings[0].contains("1 MCP servers"), "{warnings:?}");
    }

    #[tokio::test]
    async fn untrusted_project_deny_applies() {
        let folders = Folders::new();
        folders.owner_rules(json!({"rules":[{"decision":"allow","kind":"exec","command":"cat"}]}));
        folders.write(rm_and_cat());
        let policy = folders.policy().await;
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "cat secret.txt"),
            Decision::Deny
        );
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "cat notes.txt"),
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn edited_project_file_loses_trust() {
        let folders = Folders::new();
        folders.write(rm_and_cat());
        folders.trust_current();
        assert_eq!(
            status(&folders.workspace, &folders.data).trust,
            Trust::Trusted
        );
        assert_eq!(
            folders
                .policy()
                .await
                .decide(ApprovalKind::Exec, "rm -rf build"),
            Decision::Allow
        );
        folders.write(json!({"policy":{"rules":[
            {"decision":"allow","kind":"exec","command":"rm"},
            {"decision":"allow","kind":"exec","command":"curl"},
            {"decision":"deny","kind":"exec","command":"cat secret.txt"}
        ]}}));
        let now = status(&folders.workspace, &folders.data);
        let Trust::Changed { diff } = &now.trust else {
            panic!("{:?}", now.trust)
        };
        assert!(
            diff.lines()
                .any(|line| line.starts_with('+') && line.contains(r#""command": "curl""#)),
            "{diff}"
        );
        assert_eq!(now.waiting, vec!["2 allow rules"]);
        let policy = folders.policy().await;
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "rm -rf build"),
            Decision::Ask
        );
        assert_eq!(policy.decide(ApprovalKind::Exec, "curl x"), Decision::Ask);
        assert!(policy.warnings[0].contains("changed after you trusted"));
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "cat secret.txt"),
            Decision::Deny
        );
        // Untrusting removes the record; the folder name is the canonical one status shows.
        assert!(untrust(&folders.data, &now.project).unwrap());
        assert!(!untrust(&folders.data, &now.project).unwrap());
        assert_eq!(status(&folders.workspace, &folders.data).trust, Trust::None);
        assert!(trusted(&folders.data).unwrap().is_empty());
    }

    #[tokio::test]
    async fn project_deny_beats_user_allow_in_ordered_mode() {
        let folders = Folders::new();
        folders.owner_rules(json!({"mode":"ordered","rules":[
            {"decision":"allow","kind":"exec","command":"git"},
            {"decision":"deny","kind":"exec","command":"git push"}
        ]}));
        folders.write(json!({"policy":{"mode":"ordered","rules":[
            {"decision":"allow","kind":"exec","command":"git status"},
            {"decision":"ask","kind":"exec","command":"git status"}
        ]}}));
        folders.trust_current();
        let policy = folders.policy().await;
        // The owner's first match allows `git push`; that is the owner's own choice.
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "git push"),
            Decision::Allow
        );
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "git status"),
            Decision::Allow
        );
        folders.write(json!({"policy":{"mode":"ordered","rules":[
            {"decision":"allow","kind":"exec","command":"git"},
            {"decision":"deny","kind":"exec","command":"git log"}
        ]}}));
        // Untrusted now, and its later deny still applies over the owner's earlier allow.
        let policy = folders.policy().await;
        assert_eq!(policy.decide(ApprovalKind::Exec, "git log"), Decision::Deny);
        folders.trust_current();
        // Trusted, the project's own first match allows, and the owner still allows.
        let policy = folders.policy().await;
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "git log"),
            Decision::Allow
        );
        // A project deny from its own first match beats the owner's allow.
        folders.write(json!({"policy":{"mode":"ordered","rules":[
            {"decision":"deny","kind":"exec","command":"git status"},
            {"decision":"allow","kind":"exec","command":"git"}
        ]}}));
        folders.trust_current();
        let policy = folders.policy().await;
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "git status"),
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn project_file_over_64k_rejected() {
        let folders = Folders::new();
        folders.owner_rules(json!({"rules":[{"decision":"allow","kind":"exec","command":"ls"}]}));
        let mut body = rm_and_cat().to_string();
        body.push_str(&" ".repeat(MAX_PROJECT_BYTES));
        std::fs::write(folders.file(), body).unwrap();
        assert_eq!(
            try_load(&folders.workspace, &folders.data).unwrap_err(),
            ProjectError::TooLarge
        );
        // A file that cannot be used makes everything ask, even the owner's allows.
        let policy = folders.policy().await;
        assert_eq!(policy.decide(ApprovalKind::Exec, "ls"), Decision::Ask);
        assert!(policy.error.as_deref().unwrap().contains("64 KiB"));
        let now = status(&folders.workspace, &folders.data);
        assert!(now.exists && now.error.is_some() && now.sha256.is_none());
        assert_eq!(
            trust(&folders.workspace, &folders.data, "anything").unwrap_err(),
            ProjectError::TooLarge
        );
        // Exactly 64 KiB is fine.
        let mut body = rm_and_cat().to_string();
        let pad = MAX_PROJECT_BYTES - body.len();
        body.push_str(&" ".repeat(pad));
        std::fs::write(folders.file(), body).unwrap();
        assert!(try_load(&folders.workspace, &folders.data).is_ok());
    }

    #[tokio::test]
    async fn symlinked_project_file_rejected() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let folders = Folders::new();
            let outside = folders.root.join("outside.json");
            std::fs::write(&outside, rm_and_cat().to_string()).unwrap();
            symlink(&outside, folders.file()).unwrap();
            assert_eq!(
                try_load(&folders.workspace, &folders.data).unwrap_err(),
                ProjectError::NotRegular
            );
            assert_eq!(
                folders.policy().await.decide(ApprovalKind::Exec, "rm x"),
                Decision::Ask
            );
            // A linked .blackwall folder is refused too.
            std::fs::remove_dir_all(folders.workspace.join(PROJECT_DIRECTORY)).unwrap();
            let elsewhere = folders.root.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::fs::write(elsewhere.join(PROJECT_FILE), rm_and_cat().to_string()).unwrap();
            symlink(&elsewhere, folders.workspace.join(PROJECT_DIRECTORY)).unwrap();
            assert_eq!(
                try_load(&folders.workspace, &folders.data).unwrap_err(),
                ProjectError::NotRegular
            );
            // So is a folder where the file should be.
            std::fs::remove_file(folders.workspace.join(PROJECT_DIRECTORY)).unwrap();
            std::fs::create_dir_all(folders.file()).unwrap();
            assert_eq!(
                try_load(&folders.workspace, &folders.data).unwrap_err(),
                ProjectError::NotRegular
            );
        }
    }

    #[test]
    fn project_cannot_raise_limits_or_subagent_tools() {
        let folders = Folders::new();
        for (body, reason) in [
            (
                json!({"limits":{"max_output_bytes":2_000_000}}),
                "max_output_bytes",
            ),
            (
                json!({"limits":{"subagents":{"max_steps":51}}}),
                "max_steps",
            ),
            (
                json!({"limits":{"subagents":{"tools":["write_file"]}}}),
                "subagents.tools",
            ),
            (
                json!({"limits":{"subagents":{"tools":["run_command"]}}}),
                "subagents.tools",
            ),
            (
                json!({"limits":{"subagents":{"runs_on":"local"}}}),
                "runs_on",
            ),
            (json!({"instructions":{"max_file_kib":65}}), "max_file_kib"),
            (
                json!({"instructions":{"max_total_kib":512}}),
                "max_total_kib",
            ),
            (
                json!({"instructions":{"extra_files":["../outside.md"]}}),
                "extra_files",
            ),
            (
                json!({"instructions":{"extra_files":["/etc/passwd"]}}),
                "extra_files",
            ),
            (
                json!({"mcp":{"servers":[{"name":"x","command":["sh"]}]}}),
                "command",
            ),
            (
                json!({"mcp":{"servers":[{"name":"x","url":"https://a.example","secrets":["K"]}]}}),
                "secrets",
            ),
            (
                json!({"mcp":{"servers":[{"name":"x","url":"http://a.example"}]}}),
                "https",
            ),
            (
                json!({"mcp":{"servers":[{"name":"x","url":"https://u:p@a.example"}]}}),
                "credentials",
            ),
            (
                json!({"policy":{"rules":[{"decision":"allow","kind":"exec","command":"ls","project":"/x"}]}}),
                "name a project",
            ),
            (json!({"unknown":true}), "unknown"),
        ] {
            folders.write(body.clone());
            let error = try_load(&folders.workspace, &folders.data).unwrap_err();
            assert!(error.to_string().contains(reason), "{body}: {error}");
        }
        folders.write(json!({
            "limits":{"max_output_bytes":8192,"subagents":{"max_steps":3,"tools":["read_file"]}},
            "instructions":{"max_file_kib":4,"max_total_kib":16,"extra_files":["CLAUDE.md",".github/copilot-instructions.md"]},
            "mcp":{"servers":[{"name":"local","url":"http://127.0.0.1:9000/mcp","denyTools":["drop"]}]}
        }));
        folders.trust_current();
        let (config, trust, _) = load(&folders.workspace, &folders.data);
        assert_eq!(trust, Trust::Trusted);
        let limits = config.limits.unwrap();
        // The owner's value is the ceiling.
        assert_eq!(limits.output_cap(65536), 8192);
        assert_eq!(limits.output_cap(4096), 4096);
        assert_eq!(limits.subagent_steps(8), 3);
        assert_eq!(limits.subagent_steps(2), 2);
        let owner = vec!["list_files".to_owned()];
        assert!(limits.subagent_tools(&owner).is_empty());
        assert_eq!(ProjectLimits::default().subagent_tools(&owner), owner);
    }

    #[test]
    fn project_file_read_once_hash_matches_parse() {
        let folders = Folders::new();
        folders.write(rm_and_cat());
        let file = read(&folders.workspace).unwrap().unwrap();
        let bytes = std::fs::read(folders.file()).unwrap();
        assert_eq!(file.bytes, bytes);
        assert_eq!(file.sha256, hash(&bytes));
        assert_eq!(file.config, parse(&bytes).unwrap());
        let shown = status(&folders.workspace, &folders.data);
        assert_eq!(shown.sha256.as_deref(), Some(file.sha256.as_str()));
        assert_eq!(shown.content.unwrap().as_bytes(), bytes);
        // The owner trusts only what was shown: a file changed since is refused.
        folders
            .write(json!({"policy":{"rules":[{"decision":"allow","kind":"exec","command":"sh"}]}}));
        assert_eq!(
            trust(&folders.workspace, &folders.data, &file.sha256).unwrap_err(),
            ProjectError::Changed
        );
        assert_eq!(status(&folders.workspace, &folders.data).trust, Trust::None);
        let listed = trusted(&folders.data).unwrap();
        assert!(listed.is_empty());
        folders.trust_current();
        let listed = trusted(&folders.data).unwrap();
        assert_eq!(listed[0].path, folders.workspace.to_string_lossy());
        assert_eq!(
            listed[0].sha256,
            hash(&std::fs::read(folders.file()).unwrap())
        );
    }

    #[test]
    fn invalid_project_file_cannot_be_trusted_and_missing_file_is_quiet() {
        let folders = Folders::new();
        let (config, state, warnings) = load(&folders.workspace, &folders.data);
        assert_eq!((config, state), (ProjectConfig::default(), Trust::None));
        assert!(warnings.is_empty());
        let none = status(&folders.workspace, &folders.data);
        assert!(!none.exists && none.error.is_none());
        std::fs::write(folders.file(), "not json").unwrap();
        let sha = hash(b"not json");
        assert!(matches!(
            trust(&folders.workspace, &folders.data, &sha).unwrap_err(),
            ProjectError::Invalid(_)
        ));
        let (_, _, warnings) = load(&folders.workspace, &folders.data);
        assert!(warnings[0].contains("not valid"), "{warnings:?}");
        // A broken trust list is never read as trust.
        folders.write(rm_and_cat());
        std::fs::write(folders.data.join(TRUST_FILE), "{").unwrap();
        let (config, trust, warnings) = load(&folders.workspace, &folders.data);
        assert_eq!(trust, Trust::None);
        assert_eq!(config.policy.unwrap().counts(), (0, 0, 1));
        assert!(
            warnings[0].contains("trusted projects list"),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn rules_are_shown_with_badges_and_risky_project_allows_rejected() {
        let folders = Folders::new();
        folders.write(json!({"policy":{"mode":"ordered","rules":[
            {"decision":"deny","kind":"file","path":"."},
            {"decision":"ask","kind":"network","host":"*.com"},
            {"decision":"ask","kind":"network","host":"*.docs.rs"},
            {"decision":"deny","kind":"exec","tool":"github"},
            {"decision":"allow","kind":"exec","tool":"p-docs__search"},
            {"decision":"allow","kind":"exec","command":"cargo\u{202E}test"}
        ]}}));
        let shown = status(&folders.workspace, &folders.data);
        assert_eq!(shown.mode, RuleMode::Ordered);
        let row = |index: usize| {
            let rule = &shown.rules[index];
            (rule.subject.as_str(), rule.badges.join(","))
        };
        assert_eq!(row(0), (".", "matches every file".into()));
        assert_eq!(row(1), ("*.com", "any host".into()));
        assert_eq!(row(2), ("*.docs.rs", "any subdomain".into()));
        assert_eq!(row(3), ("MCP github", "your MCP server".into()));
        assert_eq!(row(4), ("MCP p-docs__search", String::new()));
        assert_eq!(row(5), ("cargo\\u{202E}test", String::new()));
        assert_eq!(shown.rules[0].decision, Decision::Deny);
        assert_eq!(shown.rules[1].kind, ApprovalKind::Network);
        // Allow rules for the owner's MCP servers or for every file are refused outright.
        for (rule, reason) in [
            (
                json!({"decision":"allow","kind":"exec","tool":"github"}),
                "p- MCP servers",
            ),
            (
                json!({"decision":"allow","kind":"exec","tool":"github__push"}),
                "p- MCP servers",
            ),
            (
                json!({"decision":"allow","kind":"file","path":"."}),
                "every file",
            ),
            (
                json!({"decision":"allow","kind":"file","path":"src/.."}),
                "every file",
            ),
        ] {
            folders.write(json!({"policy":{"rules":[rule]}}));
            let error = try_load(&folders.workspace, &folders.data).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
        }
    }

    #[test]
    fn invisible_characters_escaped_and_blank_runs_collapsed() {
        for hidden in [
            '\u{200B}',
            '\u{200F}',
            '\u{202A}',
            '\u{202E}',
            '\u{2066}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{AD}',
            '\u{E0041}',
        ] {
            let shown = escape_invisible(&format!("a{hidden}b"));
            assert_eq!(shown, format!("a\\u{{{:04X}}}b", hidden as u32));
        }
        assert_eq!(escape_invisible("plain é 中"), "plain é 中");
        assert_eq!(display_text("a\n\n\nb"), "a\n\n\nb");
        assert_eq!(
            display_text("a\n\n\n\n  \n\nb\n"),
            "a\n\n\n[3 more blank lines not shown]\nb\n"
        );
        let folders = Folders::new();
        // A byte order mark hidden in a rule, and a long run of blank lines in the whitespace.
        std::fs::write(
            folders.file(),
            format!(
                "{{{}\"policy\":{{\"rules\":[{{\"decision\":\"deny\",\"kind\":\"exec\",\"command\":\"rm\u{FEFF}x\"}}]}}}}\n",
                "\n".repeat(40)
            ),
        )
        .unwrap();
        let content = status(&folders.workspace, &folders.data).content.unwrap();
        assert!(content.contains("\\u{FEFF}"), "{content}");
        assert!(
            content.contains("[37 more blank lines not shown]"),
            "{content}"
        );
        assert!(!content.contains('\u{FEFF}'));
    }

    #[tokio::test]
    async fn diff_is_only_computed_for_status_and_bounded() {
        let folders = Folders::new();
        folders.write(rm_and_cat());
        folders.trust_current();
        let rules: Vec<_> = (0..6000)
            .map(|index| json!({"decision":"deny","kind":"exec","command":format!("tool{index}")}))
            .collect();
        let body = serde_json::to_vec(&json!({"policy":{"rules":rules[..150]}})).unwrap();
        std::fs::write(folders.file(), &body).unwrap();
        // A run never builds the diff.
        let loaded = try_load(&folders.workspace, &folders.data).unwrap();
        assert_eq!(
            loaded.trust,
            Trust::Changed {
                diff: String::new()
            }
        );
        let Trust::Changed { diff } = status(&folders.workspace, &folders.data).trust else {
            panic!()
        };
        assert!(diff.starts_with("--- trusted"), "{diff}");
        // A diff over 5000 lines is replaced by a note; the whole file is still shown.
        let long = "x\n".repeat(6000);
        assert_eq!(super::diff("", &long), DIFF_TOO_LARGE);
        assert!(super::diff("a\n", "b\n").contains("+b"));
    }

    #[tokio::test]
    async fn unreadable_trust_list_makes_every_action_ask() {
        let folders = Folders::new();
        folders.owner_rules(json!({"rules":[{"decision":"allow","kind":"exec","command":"ls"}]}));
        folders.write(rm_and_cat());
        std::fs::write(folders.data.join(TRUST_FILE), "{").unwrap();
        let policy = folders.policy().await;
        assert_eq!(policy.decide(ApprovalKind::Exec, "ls"), Decision::Ask);
        assert_eq!(
            policy.decide(ApprovalKind::Exec, "cat secret.txt"),
            Decision::Deny
        );
        let error = policy.error.unwrap();
        assert!(
            error.contains("trusted projects list could not be read"),
            "{error}"
        );
        assert!(error.contains("Every action will ask"), "{error}");
    }

    #[tokio::test]
    async fn deny_and_ask_only_project_says_its_rules_apply() {
        let folders = Folders::new();
        folders.write(json!({"policy":{"rules":[
            {"decision":"deny","kind":"exec","command":"rm"},
            {"decision":"ask","kind":"exec","command":"git push"}
        ]}}));
        let policy = folders.policy().await;
        assert_eq!(
            policy.warnings,
            vec!["2 deny/ask rules from this project's settings apply."]
        );
        let shown = status(&folders.workspace, &folders.data);
        assert_eq!(
            shown.applies,
            vec!["2 deny/ask rules from this project's settings apply"]
        );
        assert!(shown.waiting.is_empty());
    }

    #[test]
    fn windows_verbatim_prefix_is_hidden_for_display() {
        assert_eq!(display_path(r"\\?\C:\work\app"), r"C:\work\app");
        assert_eq!(
            display_path(r"\\?\UNC\server\share\app"),
            r"\\server\share\app"
        );
        assert_eq!(display_path("/work/app"), "/work/app");
    }

    #[test]
    fn oversized_trust_list_is_reported_as_too_large() {
        let folders = Folders::new();
        let filler = "x".repeat(MAX_TRUST_BYTES - 1024);
        std::fs::write(
            folders.data.join(TRUST_FILE),
            json!({"projects":[{"path":"/elsewhere","sha256":"0","content":filler}]}).to_string(),
        )
        .unwrap();
        let mut body = rm_and_cat().to_string();
        body.push_str(&" ".repeat(4096));
        std::fs::write(folders.file(), body).unwrap();
        let sha = status(&folders.workspace, &folders.data).sha256.unwrap();
        assert_eq!(
            trust(&folders.workspace, &folders.data, &sha).unwrap_err(),
            ProjectError::TrustListTooLarge
        );
    }
}
