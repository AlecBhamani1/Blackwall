use blackwall_core::command_tools::Isolation;
use blackwall_core::instructions::InstructionSettings;
use blackwall_core::sandbox::{ExecutionProfile, SandboxStatus};
use blackwall_core::subagents::{RunsOn, SubagentSettings};
use blackwall_core::{
    connection::normalize_endpoint,
    storage::LocalStore,
    telemetry::{ExportConfig, Protocol, Retention},
};
use serde_json::{json, Value};

const SETTINGS_KEY: &str = "cli-settings";
const DEFAULT_ENDPOINT: &str = "http://localhost:11434/v1";
const DEFAULT_MODEL: &str = "llama3.2";

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub endpoint: String,
    pub model: String,
    pub web_enabled: bool,
    pub execution_profile: blackwall_core::sandbox::ExecutionProfile,
    pub command_network: bool,
    /// Where owner command tools run; outside the sandbox (always ask) unless set otherwise.
    pub command_tools_isolation: Isolation,
    pub memory_enabled: bool,
    pub recall_enabled: bool,
    pub budget: blackwall_core::context::ContextBudget,
    /// Local run traces; on by default, with prompts and outputs off by default.
    pub trace: blackwall_core::trace::TraceOptions,
    /// Read `<tool_call>` blocks the model writes as text. Off by default; such calls always ask.
    pub text_tool_calls: bool,
    /// OTLP export; off until an endpoint is set. Header values are in the credential store.
    pub export: ExportConfig,
    /// How long usage metrics are kept; 400 days by default.
    pub retention: Retention,
    /// Child agents' steps, tools, and where they run.
    pub subagents: SubagentSettings,
    /// Guidance caps and the extra files read after `AGENTS.md`.
    pub instructions: InstructionSettings,
}

/// The export and retention settings saved under `telemetryExport` and `usageRetention`.
fn telemetry_options(
    object: &serde_json::Map<String, Value>,
    invalid: &str,
) -> Result<(ExportConfig, Retention), String> {
    // Fields from a newer version are ignored; a field that could hold a header value is not.
    let export = object
        .get("telemetryExport")
        .map(|value| ExportConfig::from_saved(value).map_err(|_| invalid.to_owned()))
        .transpose()?
        .unwrap_or_default();
    let retention = object
        .get("usageRetention")
        .map(|value| serde_json::from_value(value.clone()).map_err(|_| invalid.to_owned()))
        .transpose()?
        .unwrap_or_default();
    Ok((export, retention))
}

/// Traces are on unless the owner turned them off.
fn trace_options(
    object: &serde_json::Map<String, Value>,
    invalid: &str,
) -> Result<blackwall_core::trace::TraceOptions, String> {
    let flag = |key: &str, default: bool| {
        object
            .get(key)
            .map(|value| value.as_bool().ok_or_else(|| invalid.to_owned()))
            .transpose()
            .map(|value| value.unwrap_or(default))
    };
    Ok(blackwall_core::trace::TraceOptions {
        enabled: flag("tracesEnabled", true)?,
        content: flag("traceContent", false)?,
    })
}

impl Settings {
    pub fn load(store: &LocalStore) -> Result<Self, String> {
        load_with_environment(store, |key| std::env::var(key).ok())
    }

    /// Check all fields before changing either normalized value.
    pub fn validate(&mut self) -> Result<(), String> {
        self.budget.validate().map_err(|error| error.to_string())?;
        let endpoint = validated_endpoint(&self.endpoint)?;
        let model = validated_model(&self.model)?;
        let export = self.export.validated().map_err(|error| error.to_string())?;
        self.subagents.validate()?;
        self.instructions.validate()?;
        self.endpoint = endpoint;
        self.model = model;
        self.export = export;
        Ok(())
    }

    pub fn save(&self, store: &LocalStore) -> Result<(), String> {
        let mut validated = self.clone();
        validated.validate()?;
        let body = json!({
            "endpoint": validated.endpoint,
            "model": validated.model,
            "webEnabled": validated.web_enabled,
            "executionProfile": validated.execution_profile,
            "commandNetwork": validated.command_network,
            "commandToolsIsolation": validated.command_tools_isolation,
            "memoryEnabled": validated.memory_enabled,
            "recallEnabled": validated.recall_enabled,
            "contextBudget": validated.budget,
            "tracesEnabled": validated.trace.enabled,
            "traceContent": validated.trace.content,
            "textToolCalls": validated.text_tool_calls,
            "telemetryExport": validated.export,
            "usageRetention": validated.retention,
            "subagents": validated.subagents,
            "instructions": validated.instructions,
        });
        store
            .save_setting(SETTINGS_KEY, &body.to_string())
            .map_err(|error| error.to_string())
    }

    pub fn set(&mut self, store: &LocalStore, key: &str, value: &str) -> Result<(), String> {
        self.set_with_persistence(key, value, |updated| updated.save(store))
    }

    /// [`Settings::set`], checking a sandboxed `profile` against `sandbox`, which is `None` where
    /// nothing is gated (every OS but Linux). Workspace-write is refused where it can't run, with
    /// the reason.
    /// Read-only is always accepted, since it is the safe default: when the sandbox is unavailable
    /// the note comes back as a warning, and commands fail closed until it is fixed.
    pub fn set_with_sandbox(
        &mut self,
        store: &LocalStore,
        key: &str,
        value: &str,
        sandbox: impl FnOnce() -> Option<SandboxStatus>,
    ) -> Result<Option<&'static str>, String> {
        let mut warning = None;
        if key == "profile" {
            let profile: ExecutionProfile = value.parse()?;
            if profile != ExecutionProfile::Unsandboxed {
                if let Some(status) = sandbox() {
                    if profile == ExecutionProfile::WorkspaceWrite && !status.workspace_write {
                        return Err(status
                            .note
                            .unwrap_or(blackwall_core::sandbox::WORKSPACE_WRITE_UNAVAILABLE)
                            .into());
                    }
                    if !status.available {
                        warning = status.note;
                    }
                }
            }
        }
        self.set(store, key, value)?;
        Ok(warning)
    }

    fn set_with_persistence(
        &mut self,
        key: &str,
        value: &str,
        persist: impl FnOnce(&Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut updated = self.clone();
        match key {
            "endpoint" => updated.endpoint = value.into(),
            "model" => updated.model = value.into(),
            "web" => updated.web_enabled = on_or_off(value)?,
            "profile" => updated.execution_profile = value.parse()?,
            "command-network" => updated.command_network = on_or_off(value)?,
            "command-tools-isolation" => updated.command_tools_isolation = value.parse()?,
            "memory" => updated.memory_enabled = on_or_off(value)?,
            "recall" => updated.recall_enabled = on_or_off(value)?,
            "traces" => updated.trace.enabled = on_or_off(value)?,
            "trace-content" => updated.trace.content = on_or_off(value)?,
            "text-tool-calls" => updated.text_tool_calls = on_or_off(value)?,
            "otlp-endpoint" => {
                updated.export.endpoint = match value.trim() {
                    "off" => None,
                    endpoint => Some(endpoint.into()),
                }
            }
            "otlp-protocol" => {
                updated.export.protocol = match value.trim() {
                    "json" | "http/json" => Protocol::HttpJson,
                    "protobuf" | "http/protobuf" => Protocol::HttpProtobuf,
                    _ => return Err("Choose json or protobuf.".into()),
                }
            }
            "otlp-signals" => {
                let mut signals = (false, false);
                for signal in value.split(',').map(str::trim) {
                    match signal {
                        "traces" => signals.0 = true,
                        "metrics" => signals.1 = true,
                        _ => return Err("Choose traces, metrics, or traces,metrics.".into()),
                    }
                }
                (updated.export.traces, updated.export.metrics) = signals;
            }
            "otlp-include-project" => updated.export.include_project = on_or_off(value)?,
            "usage-retention-days" => updated.retention = value.parse()?,
            "context" => {
                updated.budget.context_window = value
                    .parse()
                    .map_err(|_| "Enter a context limit in tokens.")?
            }
            "output" => {
                updated.budget.output_tokens = value
                    .parse()
                    .map_err(|_| "Enter an output reserve in tokens.")?
            }
            "compact" => updated.budget.auto_compact = on_or_off(value)?,
            "output-bytes" => {
                updated.budget.max_output_bytes = value
                    .parse()
                    .ok()
                    .filter(|n| blackwall_core::context::OUTPUT_BYTES_RANGE.contains(n))
                    .ok_or("Enter a command output cap between 4096 and 1048576 bytes.")?
            }
            "subagent-steps" => {
                updated.subagents.max_steps = value
                    .parse()
                    .ok()
                    .filter(|n| (1..=50).contains(n))
                    .ok_or("Enter a subagent step limit between 1 and 50.")?
            }
            "subagent-tools" => {
                updated.subagents.tools = match value.trim() {
                    "none" => vec![],
                    list => list.split(',').map(|tool| tool.trim().to_owned()).collect(),
                }
            }
            "subagent-runs-on" => {
                updated.subagents.runs_on = match value.trim() {
                    "local" => RunsOn::Local,
                    tool => RunsOn::Tool(tool.strip_prefix("tool:").unwrap_or(tool).to_owned()),
                }
            }
            "instruction-file-kib" => {
                updated.instructions.max_file_kib = value
                    .parse()
                    .ok()
                    .filter(|n| blackwall_core::instructions::FILE_KIB_RANGE.contains(n))
                    .ok_or("Enter an instruction file limit between 1 and 64 KiB.")?
            }
            "instruction-total-kib" => {
                updated.instructions.max_total_kib = value
                    .parse()
                    .ok()
                    .filter(|n| blackwall_core::instructions::TOTAL_KIB_RANGE.contains(n))
                    .ok_or("Enter a total instruction limit between 8 and 256 KiB.")?
            }
            "instruction-files" => {
                updated.instructions.extra_files = match value.trim() {
                    "none" => vec![],
                    list => list.split(',').map(|name| name.trim().to_owned()).collect(),
                }
            }
            "max-skill-kib" => {
                updated.instructions.max_skill_kib = match value.trim().to_ascii_lowercase().as_str() {
                    "auto" => None,
                    value => Some(
                        value
                            .parse()
                            .ok()
                            .filter(|n| blackwall_core::instructions::SKILL_KIB_RANGE.contains(n))
                            .ok_or("Enter auto or a skill size limit between 32 and 256 KiB.")?,
                    ),
                }
            }
            "iterations" => {
                updated.budget.max_iterations = value
                    .parse()
                    .ok()
                    .filter(|n| (1..=100).contains(n))
                    .ok_or("Enter an iteration limit between 1 and 100.")?
            }
            "timeout" => {
                updated.budget.command_timeout_seconds = value
                    .parse()
                    .ok()
                    .filter(|n| (10..=3600).contains(n))
                    .ok_or("Enter a command time limit between 10 and 3,600 seconds.")?
            }
            _ => return Err(
                "Choose a setting: model, endpoint, profile, command-network, command-tools-isolation, web, memory, recall, traces, trace-content, text-tool-calls, context, output, output-bytes, compact, iterations, timeout, subagent-steps, subagent-tools, subagent-runs-on, instruction-file-kib, instruction-total-kib, instruction-files, max-skill-kib, otlp-endpoint, otlp-protocol, otlp-signals, otlp-include-project, or usage-retention-days."
                    .into(),
            ),
        }
        updated.validate()?;
        persist(&updated)?;
        *self = updated;
        Ok(())
    }

    pub fn describe(&self) -> String {
        // Public fields also serve setup's staged edits. Never print an unvalidated
        // endpoint or terminal control characters from an unfinished edit.
        let endpoint =
            validated_endpoint(&self.endpoint).unwrap_or_else(|_| "[invalid endpoint]".into());
        let model = validated_model(&self.model).unwrap_or_else(|_| "[invalid model]".into());
        format!(
            "model: {model}\nendpoint: {endpoint}\nweb: {}\nmemory: {}\nagent recall: {}\ntraces: {}\ntrace content: {}\ntext tool calls: {}\ncontext: {} tokens\noutput: {} tokens\nautomatic compaction: {}\niterations: {}\ncommand time limit: {} seconds\ncommand output cap: {} bytes\nsubagents: {}\ninstructions: {}\nmax skill size: {}\nexecution profile: {}\ncommand network: {}\ncommand tools: {}\notlp export: {}\nusage history: {}",
            if self.web_enabled { "on" } else { "off" },
            if self.memory_enabled { "on" } else { "off" },
            if self.recall_enabled { "on" } else { "off" },
            if self.trace.enabled { "on" } else { "off" },
            if self.trace.content { "on" } else { "off" },
            if self.text_tool_calls { "on (always ask)" } else { "off" },
            self.budget.context_window, self.budget.output_tokens,
            if self.budget.auto_compact { "on" } else { "off" },
            self.budget.max_iterations, self.budget.command_timeout_seconds,
            self.budget.max_output_bytes,
            describe_subagents(&self.subagents),
            describe_instructions(&self.instructions),
            self.instructions.max_skill_kib.map_or_else(
                || format!("Auto ({} KiB)", self.instructions.effective_skill_kib(self.budget.context_window)),
                |kib| format!("{kib} KiB"),
            ),
            self.execution_profile.name(),
            if self.execution_profile == blackwall_core::sandbox::ExecutionProfile::Unsandboxed { "on (unsandboxed)" } else if self.command_network { "on" } else { "off" },
            match self.command_tools_isolation {
                Isolation::Outside => "outside the sandbox (always ask)",
                Isolation::Sandboxed => "sandboxed (Commands profile)",
            },
            describe_export(&self.export),
            self.retention,
        )
    }

    /// [`Self::describe`] for `bw status`. Until setup or `bw config` saves settings, the model
    /// and endpoint are only defaults, so they show as not set.
    pub fn describe_status(&self, store: &LocalStore) -> Result<String, String> {
        let description = self.describe();
        if store
            .setting(SETTINGS_KEY)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(description);
        }
        Ok(description
            .lines()
            .map(|line| {
                if line.starts_with("model: ") {
                    "model: not set"
                } else if line.starts_with("endpoint: ") {
                    "endpoint: not set"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// One line for the instruction settings.
fn describe_instructions(instructions: &InstructionSettings) -> String {
    let files = if instructions.extra_files.is_empty() {
        "none".to_owned()
    } else {
        instructions.extra_files.join(",")
    };
    format!(
        "{} KiB per file, {} KiB total; extra files: {files}",
        instructions.max_file_kib, instructions.max_total_kib
    )
}

/// One line for the subagent settings.
fn describe_subagents(subagents: &SubagentSettings) -> String {
    let tools = if subagents.tools.is_empty() {
        "none".to_owned()
    } else {
        subagents.tools.join(",")
    };
    match &subagents.runs_on {
        RunsOn::Local => format!(
            "{} steps; tools: {tools}; runs on: local model",
            subagents.max_steps
        ),
        RunsOn::Tool(name) => {
            format!("runs on: the {name} tool (always asks; sends data off this device)")
        }
    }
}

/// A warning when export would send prompts and outputs, or `None`.
pub fn content_warning(settings: &Settings) -> Option<String> {
    let export = &settings.export;
    (export.is_on() && export.traces && settings.trace.enabled && settings.trace.content).then(
        || {
            format!(
                "Warning: trace content is on, so prompts, outputs, and tool data are sent to {}.",
                export
                    .origin()
                    .unwrap_or_else(|_| "the OTLP endpoint".into())
            )
        },
    )
}

/// One line for the export settings; header names only, never values.
fn describe_export(export: &ExportConfig) -> String {
    let Some(endpoint) = export
        .endpoint
        .as_deref()
        .and_then(|endpoint| blackwall_core::telemetry::export::normalize_endpoint(endpoint).ok())
    else {
        return "off".into();
    };
    let signals: Vec<_> = [("traces", export.traces), ("metrics", export.metrics)]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect();
    let origin = export.origin().ok();
    let names: Vec<String> = export
        .headers
        .iter()
        .map(|header| {
            if Some(&header.origin) == origin.as_ref() {
                header.name.clone()
            } else {
                format!(
                    "{} (saved for {}: export is paused until you enter it again or remove it)",
                    header.name, header.origin
                )
            }
        })
        .collect();
    let headers = if names.is_empty() {
        String::new()
    } else {
        format!("; headers: {}", names.join(", "))
    };
    format!(
        "{endpoint} ({}; {}{headers}; project names: {})",
        export.protocol.as_str(),
        signals.join(", "),
        if export.include_project { "on" } else { "off" }
    )
}

/// An injected environment lookup keeps precedence tests independent of the
/// developer's environment and avoids mutating process-wide environment variables.
fn load_with_environment(
    store: &LocalStore,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<Settings, String> {
    if let Some(body) = store
        .setting(SETTINGS_KEY)
        .map_err(|error| error.to_string())?
    {
        return saved_settings(&body);
    }

    let preferences: Value = store
        .setting("preferences")
        .map_err(|error| error.to_string())?
        .map(|body| {
            serde_json::from_str(&body)
                .map_err(|_| "The saved desktop preferences are invalid.".to_owned())
        })
        .transpose()?
        .unwrap_or_else(|| json!({}));
    let object = preferences
        .as_object()
        .ok_or("The saved desktop preferences are invalid.")?;
    let endpoint = preference_text(object.get("endpoint"), "endpoint")?
        .or_else(|| environment_value(&environment, "BLACKWALL_MODEL_ENDPOINT"))
        .or_else(|| environment_value(&environment, "OLLAMA_HOST"))
        .unwrap_or_else(|| DEFAULT_ENDPOINT.into());
    let model = preference_text(object.get("model"), "model")?
        .or_else(|| environment_value(&environment, "BLACKWALL_MODEL"))
        .unwrap_or_else(|| DEFAULT_MODEL.into());
    let memory_enabled = match object.get("memoryEnabled") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or("The saved desktop memory setting is invalid.")?,
    };
    let mut settings = Settings {
        endpoint,
        model,
        web_enabled: false,
        execution_profile: Default::default(),
        command_network: false,
        command_tools_isolation: blackwall_core::command_tools::isolation_from_preferences(object)
            .map_err(|_| "The saved desktop command tools setting is invalid.")?,
        memory_enabled,
        recall_enabled: object
            .get("recallEnabled")
            .map(|v| v.as_bool().ok_or("Invalid recall setting."))
            .transpose()?
            .unwrap_or(false),
        trace: trace_options(object, "Invalid trace setting.")?,
        text_tool_calls: false,
        export: telemetry_options(object, "The saved desktop telemetry settings are invalid.")?.0,
        retention: telemetry_options(object, "The saved desktop telemetry settings are invalid.")?
            .1,
        subagents: SubagentSettings::from_preferences(object)
            .map_err(|_| "The saved desktop subagent settings are invalid.")?,
        instructions: InstructionSettings::from_preferences(object)
            .map_err(|_| "The saved desktop instruction settings are invalid.")?,
        budget: blackwall_core::context::ContextBudget {
            context_window: object
                .get("contextWindow")
                .map(|v| v.as_u64().ok_or("Invalid context limit."))
                .transpose()?
                .unwrap_or(32000),
            output_tokens: object
                .get("outputTokens")
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("Invalid output reserve.")
                })
                .transpose()?
                .unwrap_or_else(|| {
                    (object
                        .get("contextWindow")
                        .and_then(Value::as_u64)
                        .unwrap_or(32000)
                        / 4)
                    .min(4096) as u32
                }),
            auto_compact: object
                .get("autoCompact")
                .map(|v| v.as_bool().ok_or("Invalid automatic compaction setting."))
                .transpose()?
                .unwrap_or(false),
            max_iterations: object
                .get("maxIterations")
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("Invalid iteration limit.")
                })
                .transpose()?
                .unwrap_or(20),
            command_timeout_seconds: object
                .get("commandTimeoutSeconds")
                .map(|v| v.as_u64().ok_or("Invalid command time limit."))
                .transpose()?
                .unwrap_or(120),
            max_output_bytes: object
                .get("maxOutputBytes")
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("Invalid output cap.")
                })
                .transpose()?
                .unwrap_or(blackwall_core::context::DEFAULT_OUTPUT_BYTES),
        },
    };
    settings.validate()?;
    Ok(settings)
}

fn saved_settings(body: &str) -> Result<Settings, String> {
    let invalid = "The saved CLI settings are invalid.";
    let value: Value = serde_json::from_str(body).map_err(|_| invalid)?;
    let object = value.as_object().ok_or(invalid)?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "endpoint"
                | "model"
                | "webEnabled"
                | "executionProfile"
                | "commandNetwork"
                | "commandToolsIsolation"
                | "memoryEnabled"
                | "recallEnabled"
                | "contextBudget"
                | "tracesEnabled"
                | "traceContent"
                | "textToolCalls"
                | "telemetryExport"
                | "usageRetention"
                | "subagents"
                | "instructions"
        )
    }) {
        return Err(invalid.into());
    }
    let (export, retention) = telemetry_options(object, invalid)?;
    let mut settings = Settings {
        execution_profile: object
            .get("executionProfile")
            .map(|v| serde_json::from_value(v.clone()).map_err(|_| invalid))
            .transpose()?
            .unwrap_or_default(),
        command_network: object
            .get("commandNetwork")
            .map(|v| v.as_bool().ok_or(invalid))
            .transpose()?
            .unwrap_or(false),
        command_tools_isolation: object
            .get("commandToolsIsolation")
            .map(|v| serde_json::from_value(v.clone()).map_err(|_| invalid))
            .transpose()?
            .unwrap_or_default(),
        budget: object
            .get("contextBudget")
            .map(|value| serde_json::from_value(value.clone()).map_err(|_| invalid))
            .transpose()?
            .unwrap_or_default(),
        endpoint: object
            .get("endpoint")
            .and_then(Value::as_str)
            .ok_or(invalid)?
            .into(),
        model: object
            .get("model")
            .and_then(Value::as_str)
            .ok_or(invalid)?
            .into(),
        web_enabled: object
            .get("webEnabled")
            .and_then(Value::as_bool)
            .ok_or(invalid)?,
        recall_enabled: object
            .get("recallEnabled")
            .map(|v| v.as_bool().ok_or(invalid))
            .transpose()?
            .unwrap_or(false),
        memory_enabled: object
            .get("memoryEnabled")
            .and_then(Value::as_bool)
            .ok_or(invalid)?,
        trace: trace_options(object, invalid)?,
        text_tool_calls: object
            .get("textToolCalls")
            .map(|v| v.as_bool().ok_or(invalid))
            .transpose()?
            .unwrap_or(false),
        export,
        retention,
        subagents: object
            .get("subagents")
            .map(|value| serde_json::from_value(value.clone()).map_err(|_| invalid))
            .transpose()?
            .unwrap_or_default(),
        instructions: object
            .get("instructions")
            .map(|value| serde_json::from_value(value.clone()).map_err(|_| invalid))
            .transpose()?
            .unwrap_or_default(),
    };
    settings
        .validate()
        .map_err(|error| format!("The saved CLI settings are invalid. {error}"))?;
    Ok(settings)
}

fn preference_text(value: Option<&Value>, key: &str) -> Result<Option<String>, String> {
    value
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("The saved desktop {key} setting is invalid."))
        })
        .transpose()
}

fn environment_value(environment: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    environment(key).filter(|value| !value.trim().is_empty())
}

fn validated_endpoint(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err("Enter a model endpoint without control characters.".into());
    }
    normalize_endpoint(value).map_err(|error| error.to_string())
}

fn validated_model(value: &str) -> Result<String, String> {
    let model = value.trim();
    if model.is_empty() || model.len() > 256 || value.chars().any(char::is_control) {
        return Err("Choose a model id of 1 to 256 bytes without control characters.".into());
    }
    Ok(model.into())
}

fn on_or_off(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("Choose on or off.".into()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            Self(std::env::temp_dir().join(format!(
                "blackwall-cli-settings-{}-{}-{}",
                std::process::id(),
                unique.as_nanos(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed),
            )))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn defaults(store: &LocalStore) -> Settings {
        load_with_environment(store, |_| None).unwrap()
    }

    #[test]
    fn cli_sets_and_persists_command_tools_isolation() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        assert_eq!(settings.command_tools_isolation, Isolation::Outside);
        assert!(settings
            .describe()
            .contains("command tools: outside the sandbox (always ask)"));
        assert!(settings
            .set(&store, "command-tools-isolation", "inside")
            .is_err());
        settings
            .set_with_persistence("command-tools-isolation", "sandboxed", |_| Ok(()))
            .unwrap();
        assert_eq!(settings.command_tools_isolation, Isolation::Sandboxed);
        assert!(settings
            .describe()
            .contains("command tools: sandboxed (Commands profile)"));
        settings
            .set(&store, "command-tools-isolation", "sandboxed")
            .unwrap();
        assert_eq!(
            defaults(&store).command_tools_isolation,
            Isolation::Sandboxed
        );
        let saved: Value =
            serde_json::from_str(&store.setting(SETTINGS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved["commandToolsIsolation"], "sandboxed");
    }

    #[test]
    fn absent_settings_use_offline_defaults() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        assert_eq!(
            defaults(&store),
            Settings {
                endpoint: DEFAULT_ENDPOINT.into(),
                model: DEFAULT_MODEL.into(),
                web_enabled: false,
                execution_profile: Default::default(),
                command_network: false,
                command_tools_isolation: Isolation::Outside,
                memory_enabled: false,
                recall_enabled: false,
                budget: Default::default(),
                trace: blackwall_core::trace::TraceOptions {
                    enabled: true,
                    content: false,
                },
                text_tool_calls: false,
                export: Default::default(),
                retention: Default::default(),
                subagents: Default::default(),
                instructions: Default::default(),
            }
        );
        assert!(store.setting(SETTINGS_KEY).unwrap().is_none());
    }

    #[test]
    fn legacy_cli_settings_load_unchanged_with_text_tool_calls_off() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        // A beta.73 CLI settings file, written before the text tool call setting existed.
        let legacy = json!({
            "endpoint": "http://127.0.0.1:11434/v1",
            "model": "qwen3.6:35b",
            "webEnabled": false,
            "memoryEnabled": false,
            "contextBudget": blackwall_core::context::ContextBudget::default(),
        });
        store
            .save_setting(SETTINGS_KEY, &legacy.to_string())
            .unwrap();
        let mut settings = defaults(&store);
        assert!(!settings.text_tool_calls);
        assert!(settings.describe().contains("text tool calls: off"));
        settings.set(&store, "text-tool-calls", "on").unwrap();
        assert!(defaults(&store).text_tool_calls);
        assert!(settings.set(&store, "text-tool-calls", "maybe").is_err());
        settings.set(&store, "text-tool-calls", "off").unwrap();
        assert!(!defaults(&store).text_tool_calls);
    }

    #[test]
    fn desktop_preferences_precede_environment_and_cli_preferences_precede_both() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let desktop = json!({
            "endpoint": "desktop.example:11434",
            "model": "desktop-model",
            "memoryEnabled": true,
            "contextWindow": 32000,
        })
        .to_string();
        store.save_setting("preferences", &desktop).unwrap();
        let environment = |key: &str| match key {
            "BLACKWALL_MODEL_ENDPOINT" => Some("http://environment.example/v1".into()),
            "BLACKWALL_MODEL" => Some("environment-model".into()),
            _ => None,
        };
        let mut settings = load_with_environment(&store, environment).unwrap();
        assert_eq!(settings.endpoint, "http://desktop.example:11434/v1");
        assert_eq!(settings.model, "desktop-model");
        assert!(settings.memory_enabled);
        assert!(!settings.web_enabled);
        settings
            .set(&store, "endpoint", "https://cli.example/")
            .unwrap();
        settings.set(&store, "model", "cli-model").unwrap();
        settings.set(&store, "web", "on").unwrap();
        settings.set(&store, "memory", "off").unwrap();
        assert_eq!(
            load_with_environment(&store, environment).unwrap(),
            settings
        );
        assert_eq!(store.setting("preferences").unwrap(), Some(desktop));
        // Already-saved CLI settings remain usable even if an unused fallback is corrupt.
        store.save_setting("preferences", "broken").unwrap();
        assert_eq!(
            load_with_environment(&store, environment).unwrap(),
            settings
        );
    }

    #[test]
    fn endpoint_environment_precedence_ignores_empty_values() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let settings = load_with_environment(&store, |key| match key {
            "BLACKWALL_MODEL_ENDPOINT" => Some("  ".into()),
            "OLLAMA_HOST" => Some("environment.example:11434".into()),
            "BLACKWALL_MODEL" => Some(" environment-model ".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(settings.endpoint, "http://environment.example:11434/v1");
        assert_eq!(settings.model, "environment-model");
        let preferred = load_with_environment(&store, |key| match key {
            "BLACKWALL_MODEL_ENDPOINT" => Some("https://preferred.example/v1".into()),
            "OLLAMA_HOST" => Some("http://other.example/v1".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(preferred.endpoint, "https://preferred.example/v1");
    }

    #[test]
    fn validated_updates_persist_and_reload_after_reopening() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings
            .set(&store, "endpoint", " https://models.example/v1/ ")
            .unwrap();
        settings.set(&store, "model", " trimmed-model ").unwrap();
        settings.set(&store, "web", "ON").unwrap();
        settings.set(&store, "recall", "on").unwrap();
        assert!(settings.recall_enabled);
        settings.set(&store, "context", "8192").unwrap();
        settings.set(&store, "output", "1024").unwrap();
        settings.set(&store, "compact", "on").unwrap();
        settings.set(&store, "iterations", "40").unwrap();
        settings.set(&store, "timeout", "600").unwrap();
        settings.set(&store, "traces", "off").unwrap();
        settings.set(&store, "trace-content", "on").unwrap();
        assert_eq!(settings.endpoint, "https://models.example/v1");
        assert_eq!(settings.model, "trimmed-model");
        drop(store);
        let reopened = LocalStore::open(&directory.0).unwrap();
        assert_eq!(defaults(&reopened), settings);
        let saved: Value =
            serde_json::from_str(&reopened.setting(SETTINGS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved.as_object().unwrap().len(), 16);
        assert_eq!(
            (
                saved["tracesEnabled"].clone(),
                saved["traceContent"].clone()
            ),
            (json!(false), json!(true))
        );
        assert_eq!(
            saved["contextBudget"],
            json!({"contextWindow":8192,"outputTokens":1024,"autoCompact":true,"maxIterations":40,"commandTimeoutSeconds":600,"maxOutputBytes":65536})
        );
        assert!(saved.get("apiKey").is_none());
    }

    #[test]
    fn invalid_updates_leave_memory_and_saved_settings_unchanged() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings.save(&store).unwrap();
        let before = settings.clone();
        let saved = store.setting(SETTINGS_KEY).unwrap();
        for (key, value) in [
            ("endpoint", "https://user:secret@models.example/v1"),
            ("endpoint", "https://models.example/v1?api_key=secret"),
            ("endpoint", "https://models.example/v1#secret"),
            ("endpoint", "file:///tmp/models"),
            ("endpoint", "http://models.example/\nsecret"),
            ("model", " "),
            ("model", "model\u{1b}[31m"),
            ("web", "yes"),
            ("memory", "sometimes"),
            ("iterations", "0"),
            ("iterations", "101"),
            ("timeout", "9"),
            ("timeout", "3601"),
            ("apiKey", "secret"),
        ] {
            let error = settings.set(&store, key, value).unwrap_err();
            assert!(!error.contains("secret"));
            assert_eq!(settings, before);
            assert_eq!(store.setting(SETTINGS_KEY).unwrap(), saved);
        }
        assert!(settings.set(&store, "model", &"a".repeat(257)).is_err());
        assert_eq!(settings, before);
        assert!(settings
            .set(&store, "iterations", "101")
            .unwrap_err()
            .contains("between 1 and 100"));
        assert!(settings
            .set(&store, "timeout", "9")
            .unwrap_err()
            .contains("between 10 and 3,600 seconds"));
    }

    #[test]
    fn profile_set_refuses_sandboxed_when_unavailable() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings.set(&store, "profile", "unsandboxed").unwrap();
        let unavailable = || {
            Some(SandboxStatus {
                available: false,
                note: Some("Install Bubblewrap."),
                workspace_write: false,
            })
        };
        // Workspace-write can't run here: refused with the reason, nothing changes or is saved.
        let before = settings.clone();
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "workspace-write", unavailable),
            Err("Install Bubblewrap.".into())
        );
        assert_eq!(settings, before);
        assert_eq!(defaults(&store), before, "nothing saved");
        // Read-only is the safe default, so it is accepted, with the note as a warning.
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "read-only", unavailable),
            Ok(Some("Install Bubblewrap."))
        );
        assert_eq!(
            defaults(&store).execution_profile,
            ExecutionProfile::ReadOnly
        );
        // Unsandboxed and other settings never ask.
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "unsandboxed", || unreachable!()),
            Ok(None)
        );
        assert_eq!(
            settings.set_with_sandbox(&store, "web", "on", || unreachable!()),
            Ok(None)
        );
        // Available, Read-only only (no overlay on this Linux): its note says why.
        let read_only = || {
            Some(SandboxStatus {
                available: true,
                note: Some("No overlay here."),
                workspace_write: false,
            })
        };
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "read-only", read_only),
            Ok(None)
        );
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "workspace-write", read_only),
            Err("No overlay here.".into())
        );
        let no_note = || {
            Some(SandboxStatus {
                available: true,
                note: None,
                workspace_write: false,
            })
        };
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "workspace-write", no_note),
            Err(blackwall_core::sandbox::WORKSPACE_WRITE_UNAVAILABLE.into())
        );
        // Both run (Linux with a working overlay): accepted.
        let both = || {
            Some(SandboxStatus {
                available: true,
                note: None,
                workspace_write: true,
            })
        };
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "workspace-write", both),
            Ok(None)
        );
        assert_eq!(
            defaults(&store).execution_profile,
            ExecutionProfile::WorkspaceWrite
        );
        settings.set(&store, "profile", "read-only").unwrap();
        // Where nothing is gated (every OS but Linux), every profile is accepted as before.
        assert_eq!(
            settings.set_with_sandbox(&store, "profile", "workspace-write", || None),
            Ok(None)
        );
        assert_eq!(
            defaults(&store).execution_profile,
            ExecutionProfile::WorkspaceWrite
        );
        assert_eq!(
            blackwall_core::sandbox::selector_status(&directory.0).is_some(),
            cfg!(target_os = "linux")
        );
    }

    #[test]
    fn execution_settings_require_explicit_values_and_persist() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        assert_eq!(settings.execution_profile.name(), "read-only");
        assert!(!settings.command_network);
        settings.set(&store, "profile", "workspace-write").unwrap();
        settings.set(&store, "command-network", "on").unwrap();
        let loaded = defaults(&store);
        assert_eq!(loaded.execution_profile.name(), "workspace-write");
        assert!(loaded.command_network);
        let before = settings.clone();
        assert!(settings.set(&store, "profile", "invalid").is_err());
        assert_eq!(settings, before);
        settings.set(&store, "profile", "unsandboxed").unwrap();
        assert!(settings.describe().contains("on (unsandboxed)"));
        let old = r#"{"endpoint":"http://localhost:11434/v1","model":"fixture","webEnabled":false,"memoryEnabled":false}"#;
        assert_eq!(
            saved_settings(old).unwrap().execution_profile.name(),
            "read-only"
        );
        assert!(saved_settings(&old.replace(
            "\"webEnabled\"",
            "\"executionProfile\":\"invalid\",\"webEnabled\""
        ))
        .is_err());
    }

    #[test]
    fn failed_persistence_does_not_publish_staged_settings() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        let before = settings.clone();
        let result = settings.set_with_persistence("model", "new-model", |updated| {
            assert_eq!(updated.model, "new-model");
            Err("The local database could not save settings.".into())
        });
        assert!(result.is_err());
        assert_eq!(settings, before);
        assert!(store.setting(SETTINGS_KEY).unwrap().is_none());
    }

    #[test]
    fn corrupt_saved_settings_have_safe_errors_without_fallback() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        for body in [
            "secret invalid JSON",
            "[]",
            "{}",
            r#"{"endpoint":7,"model":"test","webEnabled":false,"memoryEnabled":false}"#,
            r#"{"endpoint":"http://localhost:11434/v1","model":"test","webEnabled":"on","memoryEnabled":false}"#,
            r#"{"endpoint":"https://user:secret@example.com/v1","model":"test","webEnabled":false,"memoryEnabled":false}"#,
            r#"{"endpoint":"http://localhost:11434/v1","model":"test","webEnabled":false,"memoryEnabled":false,"apiKey":"secret"}"#,
        ] {
            store.save_setting(SETTINGS_KEY, body).unwrap();
            let error = load_with_environment(&store, |_| None).unwrap_err();
            assert!(error.starts_with("The saved CLI settings are invalid."));
            assert!(!error.contains("secret"));
            assert_eq!(store.setting(SETTINGS_KEY).unwrap().as_deref(), Some(body));
        }
    }

    #[test]
    fn invalid_desktop_preferences_are_reported_without_echoing_them() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        for body in [
            "secret invalid JSON",
            "[]",
            r#"{"endpoint":{"secret":"hidden"}}"#,
            r#"{"model":7}"#,
            r#"{"memoryEnabled":"secret"}"#,
        ] {
            store.save_setting("preferences", body).unwrap();
            let error = load_with_environment(&store, |_| None).unwrap_err();
            assert!(error.starts_with("The saved desktop"));
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn validation_is_atomic_and_descriptions_never_echo_credentials_or_controls() {
        let mut settings = Settings {
            endpoint: "models.example".into(),
            model: "\u{1b}[31msecret".into(),
            web_enabled: true,
            execution_profile: Default::default(),
            command_network: false,
            command_tools_isolation: Default::default(),
            memory_enabled: false,
            recall_enabled: false,
            budget: Default::default(),
            trace: Default::default(),
            text_tool_calls: false,
            export: Default::default(),
            retention: Default::default(),
            subagents: Default::default(),
            instructions: Default::default(),
        };
        let before = settings.clone();
        assert!(settings.validate().is_err());
        assert_eq!(settings, before);
        settings.endpoint = "https://user:secret@models.example/v1".into();
        let description = settings.describe();
        assert!(!description.contains("secret"));
        assert!(!description.contains('\u{1b}'));
        assert!(description.contains("web: on\nmemory: off"));
    }

    #[test]
    fn legacy_preferences_load_unchanged() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        // Desktop preferences and CLI settings from before export existed.
        let desktop =
            r#"{"endpoint":"http://localhost:11434/v1","model":"m","tracesEnabled":false}"#;
        store.save_setting("preferences", desktop).unwrap();
        let from_desktop = defaults(&store);
        assert_eq!(from_desktop.export, ExportConfig::default());
        assert_eq!(from_desktop.retention, Retention::Days(400));
        assert!(!from_desktop.trace.enabled);
        let old = r#"{"endpoint":"http://localhost:11434/v1","model":"fixture","webEnabled":false,"memoryEnabled":false,"tracesEnabled":true,"traceContent":false}"#;
        store.save_setting(SETTINGS_KEY, old).unwrap();
        let loaded = defaults(&store);
        assert_eq!(loaded.export, ExportConfig::default());
        assert_eq!(loaded.retention, Retention::default());
        assert!(loaded
            .describe()
            .contains("otlp export: off\nusage history: 400 days"));
        assert_eq!(store.setting(SETTINGS_KEY).unwrap().as_deref(), Some(old));
        // Neither file knew the output cap or subagents: both keep today's behaviour.
        for settings in [&from_desktop, &loaded] {
            assert_eq!(settings.budget.max_output_bytes, 65536);
            assert_eq!(settings.subagents, SubagentSettings::default());
            assert_eq!(settings.instructions, InstructionSettings::default());
        }
        assert!(loaded.describe().contains(
            "command output cap: 65536 bytes\nsubagents: 8 steps; tools: read_file,list_files; runs on: local model\ninstructions: 8 KiB per file, 32 KiB total; extra files: none"
        ));
        // A CLI file whose contextBudget predates maxOutputBytes loads with the default cap.
        let budget = r#"{"endpoint":"http://localhost:11434/v1","model":"fixture","webEnabled":false,"memoryEnabled":false,"contextBudget":{"contextWindow":32000,"outputTokens":4096,"autoCompact":false,"maxIterations":20,"commandTimeoutSeconds":120}}"#;
        store.save_setting(SETTINGS_KEY, budget).unwrap();
        assert_eq!(defaults(&store).budget.max_output_bytes, 65536);
    }

    #[test]
    fn output_cap_and_subagent_settings_validate_and_persist() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings.set(&store, "output-bytes", "8192").unwrap();
        settings.set(&store, "subagent-steps", "20").unwrap();
        settings
            .set(&store, "subagent-tools", "read_file, search_files,shell")
            .unwrap();
        settings.set(&store, "subagent-runs-on", "review").unwrap();
        let reopened = defaults(&store);
        assert_eq!(reopened.budget.max_output_bytes, 8192);
        assert_eq!(reopened.subagents.max_steps, 20);
        assert_eq!(
            reopened.subagents.tools,
            ["read_file", "search_files", "shell"]
        );
        assert_eq!(reopened.subagents.runs_on, RunsOn::Tool("review".into()));
        assert!(reopened.describe().contains("sends data off this device"));
        let saved: Value =
            serde_json::from_str(&store.setting(SETTINGS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved["subagents"]["runsOn"], json!({"tool":"review"}));
        for (key, value) in [
            ("output-bytes", "1024"),
            ("output-bytes", "2000000"),
            ("subagent-steps", "0"),
            ("subagent-steps", "51"),
            ("subagent-tools", "spawn_agent"),
            ("subagent-tools", "read_file,read_file"),
            ("subagent-runs-on", "Not A Tool"),
        ] {
            assert!(settings.set(&store, key, value).is_err(), "{key} {value}");
        }
        assert_eq!(defaults(&store), reopened);
        settings.set(&store, "subagent-tools", "none").unwrap();
        settings.set(&store, "subagent-runs-on", "local").unwrap();
        assert!(defaults(&store).subagents.tools.is_empty());
        assert_eq!(defaults(&store).subagents.runs_on, RunsOn::Local);
    }

    #[test]
    fn instruction_settings_validate_and_persist() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        for (key, value) in [
            ("instruction-file-kib", "1"),
            ("instruction-file-kib", "64"),
            ("instruction-total-kib", "8"),
            ("instruction-total-kib", "256"),
        ] {
            settings.set(&store, key, value).unwrap();
        }
        settings
            .set(
                &store,
                "instruction-files",
                "CLAUDE.md, .github/copilot-instructions.md",
            )
            .unwrap();
        let reopened = defaults(&store);
        assert_eq!(
            reopened.instructions,
            InstructionSettings {
                max_file_kib: 64,
                max_total_kib: 256,
                extra_files: vec!["CLAUDE.md".into(), ".github/copilot-instructions.md".into()],
                max_skill_kib: None,
            }
        );
        assert!(reopened.describe().contains(
            "instructions: 64 KiB per file, 256 KiB total; extra files: CLAUDE.md,.github/copilot-instructions.md"
        ));
        let saved: Value =
            serde_json::from_str(&store.setting(SETTINGS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved["instructions"]["maxFileKib"], 64);
        assert!(saved["instructions"].get("maxSkillKib").is_none());
        settings.set(&store, "max-skill-kib", "96").unwrap();
        assert_eq!(defaults(&store).instructions.max_skill_kib, Some(96));
        settings.set(&store, "max-skill-kib", "AuTo").unwrap();
        assert_eq!(defaults(&store).instructions.max_skill_kib, None);
        let saved: Value =
            serde_json::from_str(&store.setting(SETTINGS_KEY).unwrap().unwrap()).unwrap();
        assert!(saved["instructions"].get("maxSkillKib").is_none());
        for (key, value) in [
            ("instruction-file-kib", "0"),
            ("instruction-file-kib", "65"),
            ("instruction-total-kib", "7"),
            ("instruction-total-kib", "257"),
            ("instruction-files", "../CLAUDE.md"),
            ("instruction-files", "/etc/passwd"),
            ("instruction-files", "AGENTS.md"),
            ("instruction-files", "a.md,a.md"),
            ("instruction-files", "a,b,c,d,e,f,g,h,i"),
        ] {
            assert!(settings.set(&store, key, value).is_err(), "{key} {value}");
        }
        assert_eq!(defaults(&store), reopened);
        settings.set(&store, "instruction-files", "none").unwrap();
        assert!(defaults(&store).instructions.extra_files.is_empty());
        // A saved file edited by hand is checked again when it loads.
        let mut edited = saved.clone();
        edited["instructions"]["extraFiles"] = json!(["../outside.md"]);
        store
            .save_setting(SETTINGS_KEY, &edited.to_string())
            .unwrap();
        assert!(Settings::load(&store).is_err());
        // Without a CLI file, the desktop's saved preferences are used and checked the same way.
        let desktop = TestDirectory::new();
        let store = LocalStore::open(&desktop.0).unwrap();
        let preferences = json!({"instructionMaxFileKib":4,"instructionExtraFiles":["CLAUDE.md"]});
        store
            .save_setting("preferences", &preferences.to_string())
            .unwrap();
        let loaded = defaults(&store).instructions;
        assert_eq!((loaded.max_file_kib, loaded.max_total_kib), (4, 32));
        assert_eq!(loaded.extra_files, ["CLAUDE.md"]);
        let preferences = json!({"instructionMaxTotalKib":512});
        store
            .save_setting("preferences", &preferences.to_string())
            .unwrap();
        assert!(Settings::load(&store).is_err());
    }

    #[test]
    fn otlp_settings_validate_and_persist_names_only() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        for (key, value) in [
            ("otlp-endpoint", "http://collector.example:4318"),
            ("otlp-endpoint", "https://user:secret@collector.example"),
            ("otlp-endpoint", "https://collector.example/?key=secret"),
            ("otlp-protocol", "grpc"),
            ("otlp-signals", "logs"),
            ("otlp-include-project", "yes"),
            ("usage-retention-days", "29"),
            ("usage-retention-days", "3651"),
        ] {
            let before = settings.clone();
            let error = settings.set(&store, key, value).unwrap_err();
            assert!(!error.contains("secret"), "{error}");
            assert_eq!(settings, before);
        }
        settings
            .set(
                &store,
                "otlp-endpoint",
                "https://otel.example.com/v1/traces",
            )
            .unwrap();
        settings.set(&store, "otlp-protocol", "json").unwrap();
        settings.set(&store, "otlp-signals", "metrics").unwrap();
        settings.set(&store, "otlp-include-project", "on").unwrap();
        assert_eq!(
            settings.export.endpoint.as_deref(),
            Some("https://otel.example.com/")
        );
        assert!(settings.describe().contains(
            "otlp export: https://otel.example.com/ (http/json; metrics; project names: on)"
        ));
        let reloaded = defaults(&store);
        assert_eq!(reloaded, settings);
        settings.set(&store, "otlp-endpoint", "off").unwrap();
        settings
            .set(&store, "otlp-endpoint", "http://127.0.0.1:4318")
            .unwrap();
        assert!(defaults(&store).export.is_on());
        settings.set(&store, "otlp-endpoint", "off").unwrap();
        assert!(!defaults(&store).export.is_on());
        // Unknown export fields, such as a smuggled header value, are refused when loading.
        store
            .save_setting(
                SETTINGS_KEY,
                r#"{"endpoint":"http://localhost:11434/v1","model":"m","webEnabled":false,"memoryEnabled":false,"telemetryExport":{"headerValues":{"x":"secret"}}}"#,
            )
            .unwrap();
        let error = load_with_environment(&store, |_| None).unwrap_err();
        assert!(!error.contains("secret"));
    }

    #[test]
    fn retention_setting_round_trips() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings.set(&store, "usage-retention-days", "90").unwrap();
        assert_eq!(defaults(&store).retention, Retention::Days(90));
        settings
            .set(&store, "usage-retention-days", "forever")
            .unwrap();
        assert_eq!(defaults(&store).retention, Retention::Forever);
        let saved: Value =
            serde_json::from_str(&store.setting(SETTINGS_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved["usageRetention"], json!("forever"));
        assert!(settings.describe().contains("usage history: forever"));
        // The desktop's saved value is used until the CLI saves its own.
        let other = TestDirectory::new();
        let store = LocalStore::open(&other.0).unwrap();
        store
            .save_setting("preferences", r#"{"usageRetention":30}"#)
            .unwrap();
        assert_eq!(defaults(&store).retention, Retention::Days(30));
    }

    #[test]
    fn newer_export_fields_are_ignored_but_header_values_are_refused() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        store
            .save_setting(
                SETTINGS_KEY,
                r#"{"endpoint":"http://localhost:11434/v1","model":"m","webEnabled":false,"memoryEnabled":false,"telemetryExport":{"endpoint":"https://otel.example.com/","compression":"gzip","headers":[{"name":"x-api-key","origin":"https://other.example:443"}]}}"#,
            )
            .unwrap();
        let loaded = defaults(&store);
        assert_eq!(
            loaded.export.endpoint.as_deref(),
            Some("https://otel.example.com/")
        );
        assert!(loaded
            .describe()
            .contains("x-api-key (saved for https://other.example:443: export is paused"));
    }

    #[test]
    fn content_warning_only_when_content_would_leave() {
        let directory = TestDirectory::new();
        let store = LocalStore::open(&directory.0).unwrap();
        let mut settings = defaults(&store);
        settings.set(&store, "trace-content", "on").unwrap();
        assert_eq!(content_warning(&settings), None);
        settings
            .set(&store, "otlp-endpoint", "https://otel.example.com")
            .unwrap();
        assert!(content_warning(&settings)
            .unwrap()
            .contains("prompts, outputs, and tool data are sent to https://otel.example.com:443"));
        settings.set(&store, "otlp-signals", "metrics").unwrap();
        assert_eq!(content_warning(&settings), None);
    }
}
