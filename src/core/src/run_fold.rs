//! The one authoritative fold from a run's `AgentEvent`s to the assistant message the user sees
//! (design R5-12).
//!
//! The desktop shape is the reducer in `ui/src/lib/store.ts`, with the event normalization of
//! `ui/src/lib/ipc.ts`, the error copy of `ui/src/lib/modelError.ts` and the saved shape of
//! `persistedMessages`. The CLI shape is what `src/bw/src/session.rs` saves. Both must match those
//! byte for byte as canonical JSON, including which keys are present. Lengths and offsets count
//! UTF-16 units and "whitespace" is JavaScript's, because that is what the webview measures.

use crate::{
    context::ContextError,
    model::message_form,
    protocol::{AgentEvent, ChatMessage},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Which client's assistant message a run folds to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantShape {
    Desktop,
    Cli,
}

/// Which saved UI messages a client sends to the model as the run's source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceRule {
    Desktop,
    Cli,
}

/// How a run ended. `Failed::message` is what the CLI saves; the desktop shows the copy for
/// `code` and `detail` instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEnd {
    Complete,
    Stopped,
    Failed {
        code: String,
        message: String,
        detail: Option<String>,
    },
}

/// `ipc.ts` `MAX_REASONING_CHARS`: the reasoning the webview keeps, in UTF-16 units.
const MAX_REASONING_UNITS: usize = 200_000;
/// `serverDetail`: a longer server detail is cut to 299 units and an ellipsis.
const MAX_DETAIL_UNITS: usize = 300;
/// `store.ts` `stop()` and the abort path.
const RESPONSE_STOPPED: &str = "Response stopped.";
const CHILD_STOPPED: &str = "Stopped with the parent task.";
const CHILD_ENDED: &str = "The parent task ended.";
/// `session.rs` `execute`: what an interrupted CLI run saves when it streamed no text.
const CLI_STOPPED: &str = "Task stopped. You can continue this conversation.";
const GENERIC_ERROR: &str =
    "The model could not complete the request. Try again or check the model service.";

/// The screen text of a run's turns: the agent and chat loops add `"\n\n"` before every later
/// turn while the text so far is nonempty, empty turns included (`agent.rs`, `context.rs`).
pub fn accumulate<'a>(turns: impl IntoIterator<Item = &'a str>) -> String {
    let mut acc = String::new();
    for (i, turn) in turns.into_iter().enumerate() {
        if i > 0 && !acc.is_empty() {
            acc.push_str("\n\n");
        }
        acc.push_str(turn);
    }
    acc
}

/// JavaScript's `\s` and `String.prototype.trim` set. It has U+FEFF and lacks U+0085, unlike
/// `char::is_whitespace`.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.slice(0, units)`. A cut through a surrogate pair leaves JavaScript a lone surrogate,
/// which a Rust string cannot hold; it becomes U+FFFD, which is how the webview shows it.
fn utf16_prefix(text: &str, units: usize) -> String {
    if utf16_len(text) <= units {
        return text.to_owned();
    }
    String::from_utf16_lossy(&text.encode_utf16().take(units).collect::<Vec<_>>())
}

/// Replaces each maximal run of characters matching `matches` with one space, when `replace`
/// accepts the run.
fn collapse_runs(
    text: &str,
    matches: impl Fn(char) -> bool,
    replace: impl Fn(&str) -> bool,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run_start = None;
    for (index, c) in text.char_indices() {
        match (matches(c), run_start) {
            (true, None) => run_start = Some(index),
            (false, Some(start)) => {
                let run = &text[start..index];
                out.push_str(if replace(run) { " " } else { run });
                run_start = None;
                out.push(c);
            }
            (false, None) => out.push(c),
            (true, Some(_)) => {}
        }
    }
    if let Some(start) = run_start {
        let run = &text[start..];
        out.push_str(if replace(run) { " " } else { run });
    }
    out
}

/// `store.ts` `warning.replace(/\s*\n\s*/g, ' ')`: every whitespace run holding a newline.
fn one_line(warning: &str) -> String {
    collapse_runs(warning, is_js_space, |run| run.contains('\n'))
}

/// `modelError.ts` `serverDetail`.
fn server_detail(detail: &str) -> String {
    let controls = collapse_runs(
        detail,
        |c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'),
        |_| true,
    );
    let spaces = collapse_runs(&controls, is_js_space, |_| true);
    let flat = js_trim(&spaces);
    if utf16_len(flat) > MAX_DETAIL_UNITS {
        format!("{}…", utf16_prefix(flat, MAX_DETAIL_UNITS - 1))
    } else {
        flat.to_owned()
    }
}

/// `platform.ts` `storeHelp` for the OS this build runs on.
#[cfg(target_os = "macos")]
const STORE_HELP: &str =
    "Respond to any macOS Keychain prompt, unlock your login keychain if needed";
#[cfg(target_os = "windows")]
const STORE_HELP: &str =
    "Make sure you are signed in to Windows and Credential Manager is available";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const STORE_HELP: &str = "Respond to any keyring unlock prompt and make sure GNOME Keyring, KWallet, or another Secret Service provider is running";

/// The allowlisted copy of `nativeModelError` for a known code.
fn known_error(code: &str) -> Option<String> {
    let copy = match code {
        "context_budget_invalid" => "Choose a valid model context limit, a smaller positive output reserve, 1–100 iterations, and a 10–3,600 second command time limit.",
        "context_limit" => "The estimated request exceeds the model context budget. Use /compact, increase the configured limit, or shorten the task.",
        "context_history_invalid" => "The saved model history or checkpoint is invalid. The original conversation has been retained.",
        "context_summary_invalid" => "The model returned an invalid or oversized summary. Previous context has been retained.",
        "context_nothing_to_compact" => "There is no older context to compact; the two most recent user turns are retained.",
        "agent_iteration_limit" => "The agent reached its iteration limit. Review its progress before continuing.",
        "storage_error" => "Blackwall could not access its local data. Check disk space and folder permissions, then retry saving. Keep Blackwall open to preserve unsaved work.",
        "credential_error" => {
            return Some(format!(
                "Blackwall could not access the saved key. {STORE_HELP}, then reconnect."
            ))
        }
        "model_unavailable" => "The model computer is unavailable. Open and unlock Blackwall on that computer, keep its model running, and reconnect.",
        "model_access_denied" => "Model access was refused or removed. Check your access key, or pair the computer again if its access was removed.",
        "model_timeout" => "The model took too long to respond. Check that its computer is awake and connected, then try again.",
        "model_busy" => "The model is busy. Wait for another request to finish, then try again.",
        "model_not_found" => "The model connection is no longer available. Check that its computer is open, unlocked, and awake. If access was removed, pair again.",
        "invalid_request" => "The request could not be sent. Check the connection settings, model, and attachments.",
        "model_response_too_large" => "The model response was too large. Ask for a shorter response.",
        "empty_model_response" => "The model returned no answer. Try a shorter prompt or choose another model.",
        "invalid_model_response" => "The model returned an unreadable response. Try again or check the model service.",
        _ => return None,
    };
    Some(copy.to_owned())
}

/// The error text the webview shows for a failed run: `modelError.ts` `nativeModelError`.
pub fn model_error_text(code: &str, detail: Option<&str>) -> String {
    let detail = detail.map(server_detail).unwrap_or_default();
    match (known_error(code), detail.is_empty()) {
        (Some(copy), true) => copy,
        (Some(copy), false) => format!("{copy} ({detail})"),
        (None, true) => GENERIC_ERROR.to_owned(),
        (None, false) => detail,
    }
}

/// The model source a client sends for its saved UI messages. The desktop drops `error` and
/// `sending` rows (`store.ts` `asModelMessages`); the CLI sends every row and treats `system`
/// entries as instructions, not source (`session.rs` `context` and `run`).
pub fn run_source(messages: &[Value], rule: SourceRule) -> Result<Vec<Value>, ContextError> {
    let form = |row: &Value| {
        serde_json::from_value::<ChatMessage>(row.clone())
            .map(|message| message_form(&message, &[]))
            .map_err(|_| ContextError::History)
    };
    match rule {
        SourceRule::Desktop => messages
            .iter()
            .filter(|row| !matches!(row["status"].as_str(), Some("error" | "sending")))
            .map(form)
            .collect(),
        SourceRule::Cli => Ok(messages
            .iter()
            .map(form)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| entry["role"] != "system")
            .collect()),
    }
}

/// A run's assistant message, folded one event at a time. It starts `streaming`; `finish` ends
/// it, after which further events and ends change nothing (the webview stops listening).
pub struct RunFold {
    shape: AssistantShape,
    message: Value,
}

impl RunFold {
    pub fn new(id: &str, created_at: i64, shape: AssistantShape) -> Self {
        Self {
            shape,
            message: json!({
                "id": id,
                "role": "assistant",
                "content": "",
                "attachments": [],
                "status": "streaming",
                "createdAt": created_at,
            }),
        }
    }

    pub fn apply(&mut self, event: &AgentEvent) {
        if !self.open() {
            return;
        }
        let shape = self.shape;
        let Some(message) = self.message.as_object_mut() else {
            return;
        };
        // Measured only for the events that record it: a delta must not rescan the text.
        let offset = |message: &Map<String, Value>| {
            utf16_len(
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        };
        match event {
            AgentEvent::AssistantDelta { delta, .. } => {
                if let Some(Value::String(content)) = message.get_mut("content") {
                    content.push_str(delta);
                }
            }
            _ if shape == AssistantShape::Cli => {}
            AgentEvent::ToolCall {
                tool_call_id,
                name,
                arguments,
                ..
            } => {
                let offset = offset(message);
                with_list(message, "tools", |tools| {
                    tools.push(json!({
                        "id": tool_call_id,
                        "name": name,
                        "arguments": arguments,
                        "contentOffset": offset,
                        "status": "running",
                    }))
                });
            }
            AgentEvent::ToolResult {
                tool_call_id,
                output,
                success,
                truncated,
                ..
            } => {
                let Some(Value::Array(tools)) = message.get_mut("tools") else {
                    return;
                };
                for tool in tools.iter_mut().filter(|tool| tool["id"] == *tool_call_id) {
                    tool["output"] = json!(output);
                    if *truncated {
                        tool["truncated"] = json!(true);
                    }
                    tool["status"] = json!(if *success { "complete" } else { "error" });
                }
            }
            AgentEvent::InstructionsLoaded {
                agent_id,
                sources,
                warnings,
                ..
            } => {
                let id = match agent_id.as_deref() {
                    Some(agent) if !agent.is_empty() => format!("instructions_{agent}"),
                    _ => "instructions".to_owned(),
                };
                let offset = offset(message);
                let joined = sources.join(", ");
                let path = if joined.is_empty() { "None" } else { &joined };
                let mut output = vec![format!("Loaded instructions: {path}")];
                output.extend(
                    warnings
                        .iter()
                        .map(|warning| format!("Warning: {}", one_line(warning))),
                );
                with_list(message, "tools", |tools| {
                    let offset = tools
                        .iter()
                        .find(|tool| tool["id"] == id.as_str())
                        .map_or(json!(offset), |tool| tool["contentOffset"].clone());
                    tools.retain(|tool| tool["id"] != id.as_str());
                    // `diagnostics` are for the owner's screen and never saved.
                    tools.push(json!({
                        "id": id,
                        "name": "project_instructions",
                        "contentOffset": offset,
                        "arguments": json!({"path": path}).to_string(),
                        "output": output.join("\n"),
                        "status": if warnings.is_empty() { "complete" } else { "error" },
                    }));
                });
            }
            AgentEvent::SubagentStatus {
                agent_id,
                state,
                summary,
                ..
            } => {
                with_list(message, "children", |children| {
                    children.retain(|child| child["id"] != agent_id.as_str());
                    children.push(json!({
                        "id": agent_id,
                        "state": state,
                        "summary": summary.as_deref().unwrap_or_default(),
                    }));
                });
            }
            AgentEvent::TurnComplete {
                reasoning: Some(reasoning),
                reasoning_ms,
                ..
            } if !reasoning.is_empty() => {
                message.insert(
                    "reasoning".into(),
                    json!(utf16_prefix(reasoning, MAX_REASONING_UNITS)),
                );
                match reasoning_ms {
                    Some(ms) => message.insert("reasoningMs".into(), json!(ms)),
                    None => message.remove("reasoningMs"),
                };
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, end: &RunEnd) {
        if !self.open() {
            return;
        }
        match (self.shape, end) {
            (AssistantShape::Desktop, RunEnd::Complete) if js_trim(self.content()).is_empty() => {
                self.fail(model_error_text("empty_model_response", None));
            }
            (_, RunEnd::Complete) => self.message["status"] = json!("complete"),
            (AssistantShape::Desktop, RunEnd::Stopped) => self.stop(),
            (AssistantShape::Desktop, RunEnd::Failed { code, detail, .. }) => {
                self.fail(model_error_text(code, detail.as_deref()));
            }
            (AssistantShape::Cli, RunEnd::Stopped) => self.cli_fail(CLI_STOPPED),
            (AssistantShape::Cli, RunEnd::Failed { message, .. }) => self.cli_fail(message),
        }
    }

    pub fn message(&self) -> &Value {
        &self.message
    }

    fn open(&self) -> bool {
        self.message["status"] == "streaming"
    }

    fn content(&self) -> &str {
        self.message["content"].as_str().unwrap_or_default()
    }

    /// `store.ts` `stop()`.
    fn stop(&mut self) {
        if self.content().is_empty() {
            self.message["content"] = json!(RESPONSE_STOPPED);
        }
        self.end("stopped", CHILD_STOPPED);
    }

    /// The `store.ts` catch path for a failure that was not a stop.
    fn fail(&mut self, error: String) {
        self.end("error", CHILD_ENDED);
        self.message["error"] = json!(error);
    }

    /// Status, plus running tools and children, as a desktop run ends without completing.
    fn end(&mut self, status: &str, child_summary: &str) {
        self.message["status"] = json!(status);
        if let Some(Value::Array(tools)) = self.message.get_mut("tools") {
            for tool in tools.iter_mut().filter(|tool| tool["status"] == "running") {
                tool["status"] = json!(status);
            }
        }
        if let Some(Value::Array(children)) = self.message.get_mut("children") {
            for child in children
                .iter_mut()
                .filter(|child| child["state"] == "running")
            {
                child["state"] = json!("interrupted");
                child["summary"] = json!(child_summary);
            }
        }
    }

    /// `session.rs`: a failed or interrupted run saves its streamed text, or the failure.
    fn cli_fail(&mut self, failure: &str) {
        if self.content().is_empty() {
            self.message["content"] = json!(failure);
        }
        self.message["status"] = json!("error");
    }
}

/// Changes the list at `key`, created empty on first use (`message.tools ?? []`).
fn with_list(message: &mut Map<String, Value>, key: &str, change: impl FnOnce(&mut Vec<Value>)) {
    if let Value::Array(items) = message.entry(key).or_insert_with(|| json!([])) {
        change(items);
    }
}

/// Folds a whole run. With no `end`, the run is still open (`streaming`).
pub fn fold_run(
    id: &str,
    created_at: i64,
    shape: AssistantShape,
    events: &[AgentEvent],
    end: Option<&RunEnd>,
) -> Value {
    let mut fold = RunFold::new(id, created_at, shape);
    for event in events {
        fold.apply(event);
    }
    if let Some(end) = end {
        fold.finish(end);
    }
    fold.message
}

#[cfg(test)]
#[path = "run_fold/tests.rs"]
mod tests;
