//! Stable serde contract between `blackwall-core` and its clients.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default OpenAI-compatible endpoint exposed by a local Ollama installation.
pub const DEFAULT_MODEL_ENDPOINT: &str = "http://localhost:11434/v1";

/// The maximum size accepted for one attachment at the protocol boundary.
pub const MAX_ATTACHMENT_BYTES: u64 = 15 * 1024 * 1024;

/// The maximum number of attachments accepted on one message.
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 8;

/// The maximum combined attachment size accepted on one message.
pub const MAX_ATTACHMENT_TOTAL_BYTES: u64 = 30 * 1024 * 1024;

/// A role in an OpenAI-compatible chat transcript.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// Instructions that establish the assistant's behavior.
    System,
    /// A message authored by the person using Blackwall.
    User,
    /// A message authored by the model.
    Assistant,
    /// Output returned by a tool invocation.
    Tool,
}

/// A file or image supplied alongside a user message.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// Client-generated stable identifier for this attachment.
    pub id: String,
    /// Original filename shown in the transcript.
    pub name: String,
    /// IANA media type, such as `image/png` or `text/plain`.
    pub mime_type: String,
    /// Unencoded source size in bytes.
    pub size_bytes: u64,
    /// Optional data URL carrying bytes to an image-capable local model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_url: Option<String>,
    /// Optional decoded text supplied for text-based files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_content: Option<String>,
}

impl Attachment {
    /// Validates fields that cross the desktop IPC boundary.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.id.trim().is_empty() {
            return Err(ProtocolError::MissingAttachmentId);
        }
        if self.name.trim().is_empty() {
            return Err(ProtocolError::MissingAttachmentName);
        }
        if self.mime_type.trim().is_empty() {
            return Err(ProtocolError::MissingAttachmentMediaType);
        }
        if self.size_bytes > MAX_ATTACHMENT_BYTES {
            return Err(ProtocolError::AttachmentTooLarge {
                name: self.name.clone(),
                size_bytes: self.size_bytes,
                maximum_bytes: MAX_ATTACHMENT_BYTES,
            });
        }
        Ok(())
    }
}

/// One message sent to or returned by the model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// Author role understood by OpenAI-compatible endpoints.
    pub role: MessageRole,
    /// Plain-text message content.
    pub content: String,
    /// Files and images associated with this specific message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

impl ChatMessage {
    /// Creates a plain-text chat message.
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            attachments: Vec::new(),
        }
    }
}

/// Request to generate one assistant turn.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    /// Client-generated identifier used to correlate events and ignore stale streams.
    #[serde(default)]
    pub request_id: String,
    /// Optional endpoint override; omitted requests use the client's configured endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Identifier advertised by the endpoint's `/models` route.
    #[serde(default)]
    pub model: String,
    /// Ordered transcript passed to the model.
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// Files and images attached to the newest user message.
    ///
    /// New clients should prefer message-level attachments. This field remains
    /// available for simple clients and protocol compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Sampling temperature, if explicitly selected by the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Maximum generated tokens, if explicitly selected by the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Configured context/output budget; defaults preserve older clients.
    #[serde(default)]
    pub context_budget: crate::context::ContextBudget,
    /// Separately persisted original model transcript and compacted checkpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<crate::context::ContextState>,
    /// Request summary-only compaction, without executing tools.
    #[serde(default)]
    pub compact: bool,
    /// The connection's "This model writes tool calls as text" setting. Off by default; calls
    /// read from text always ask for approval.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text_tool_calls: bool,
}

impl ChatRequest {
    /// Creates a request that delegates endpoint selection and has no attachments.
    pub fn new(
        request_id: impl Into<String>,
        model: impl Into<String>,
        messages: Vec<ChatMessage>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            endpoint: None,
            model: model.into(),
            messages,
            attachments: Vec::new(),
            temperature: None,
            max_tokens: None,
            context_budget: Default::default(),
            context_state: None,
            compact: false,
            text_tool_calls: false,
        }
    }

    /// Validates the request before any network activity begins.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.request_id.trim().is_empty() {
            return Err(ProtocolError::MissingRequestId);
        }
        if self.model.trim().is_empty() {
            return Err(ProtocolError::MissingModel);
        }
        if self.messages.is_empty() {
            return Err(ProtocolError::MissingMessages);
        }
        if let Some(temperature) = self.temperature {
            if !(0.0..=2.0).contains(&temperature) {
                return Err(ProtocolError::InvalidTemperature(temperature));
            }
        }
        self.context_budget
            .validate()
            .map_err(|_| ProtocolError::InvalidContextBudget)?;
        if self
            .max_tokens
            .is_some_and(|n| n == 0 || n > self.context_budget.output_tokens)
        {
            return Err(ProtocolError::InvalidContextBudget);
        }
        validate_attachment_group(&self.attachments)?;
        for message in &self.messages {
            validate_attachment_group(&message.attachments)?;
        }
        Ok(())
    }
}

fn validate_attachment_group(attachments: &[Attachment]) -> Result<(), ProtocolError> {
    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(ProtocolError::TooManyAttachments {
            count: attachments.len(),
            maximum: MAX_ATTACHMENTS_PER_MESSAGE,
        });
    }
    let mut total_bytes = 0_u64;
    for attachment in attachments {
        attachment.validate()?;
        total_bytes = total_bytes.saturating_add(attachment.size_bytes);
    }
    if total_bytes > MAX_ATTACHMENT_TOTAL_BYTES {
        return Err(ProtocolError::AttachmentsTooLarge {
            total_bytes,
            maximum_bytes: MAX_ATTACHMENT_TOTAL_BYTES,
        });
    }
    Ok(())
}

/// Token counts reported by a compatible model endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", from = "WireTokenUsage")]
pub struct TokenUsage {
    /// Tokens consumed by the prompt.
    pub prompt_tokens: u64,
    /// Tokens generated by the model.
    pub completion_tokens: u64,
    /// Total tokens consumed by this request.
    pub total_tokens: u64,
    /// Prompt tokens the server read from its cache; part of `prompt_tokens`.
    #[serde(skip_serializing_if = "is_zero")]
    pub cached_input_tokens: u64,
    /// Generated tokens spent on reasoning; part of `completion_tokens`.
    #[serde(skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Token usage as servers send it (`prompt_tokens_details.cached_tokens`,
/// `completion_tokens_details.reasoning_tokens`) or as Blackwall stores it.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTokenUsage {
    #[serde(default, alias = "prompt_tokens")]
    prompt_tokens: u64,
    #[serde(default, alias = "completion_tokens")]
    completion_tokens: u64,
    #[serde(default, alias = "total_tokens")]
    total_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default, alias = "prompt_tokens_details")]
    prompt_tokens_details: Option<WireTokenDetails>,
    #[serde(default, alias = "completion_tokens_details")]
    completion_tokens_details: Option<WireTokenDetails>,
}

#[derive(Default, Deserialize)]
struct WireTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<WireTokenUsage> for TokenUsage {
    fn from(wire: WireTokenUsage) -> Self {
        let cached = wire
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens)
            .unwrap_or(wire.cached_input_tokens);
        let reasoning = wire
            .completion_tokens_details
            .and_then(|details| details.reasoning_tokens)
            .unwrap_or(wire.reasoning_tokens);
        Self {
            prompt_tokens: wire.prompt_tokens,
            completion_tokens: wire.completion_tokens,
            total_tokens: wire.total_tokens,
            cached_input_tokens: cached.min(wire.prompt_tokens),
            reasoning_tokens: reasoning.min(wire.completion_tokens),
        }
    }
}

/// Complete response returned by the non-streaming command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatResponse {
    /// Identifier copied from the originating request.
    pub request_id: String,
    /// Model identifier returned by the endpoint.
    pub model: String,
    /// Final assistant message.
    pub message: ChatMessage,
    /// Endpoint-specific finish reason, such as `stop` or `length`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Token usage when reported by the endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

/// One model advertised by the configured endpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    /// Identifier accepted by a chat-completions request.
    pub id: String,
    /// Optional human-readable display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional provider or owner reported by the endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

/// Result of querying an endpoint's model catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    /// Normalized endpoint used for discovery.
    pub endpoint: String,
    /// Models available for chat, in endpoint order.
    pub models: Vec<ModelInfo>,
}

/// Correlation acknowledgement returned by the streaming desktop command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStarted {
    /// Identifier used by events emitted on `blackwall://event`.
    pub request_id: String,
}

/// An action that requires explicit user consent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Execute a shell command.
    Exec,
    /// Write, move, or delete a file.
    File,
    /// Reach a network host outside the model connection.
    Network,
    /// Grant a child agent additional capabilities.
    Subagent,
}

/// A person's answer to an approval prompt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Permit this operation once.
    Allow,
    /// Permit this operation and remember the scoped rule.
    AlwaysAllow,
    /// Refuse this operation.
    Deny,
    /// Permit this MCP tool from now on in this project, saving an owner rule.
    AllowToolInProject,
    /// Permit this MCP tool from now on in every project, saving an owner rule.
    AllowTool,
    /// Permit every tool from this MCP server from now on in this project, saving an owner rule.
    AllowServerInProject,
    /// Permit every tool from this MCP server from now on in every project, saving an owner rule.
    AllowServer,
}

/// The MCP tool an approval is for, so the owner can allow it from now on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalTool {
    /// Server name from the owner's MCP file.
    pub server: String,
    /// Namespaced `server__tool` name.
    pub tool: String,
}

/// Request to resolve an outstanding approval event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveApprovalRequest {
    /// Request whose turn is waiting.
    pub request_id: String,
    /// Unique approval identifier from the event.
    pub approval_id: String,
    /// Decision selected in the UI.
    pub decision: ApprovalDecision,
}

/// Lifecycle state of a child agent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    /// The child has been accepted but has not begun work.
    Queued,
    /// The child is actively working.
    Running,
    /// The child returned a result.
    Completed,
    /// The child stopped with a structured failure.
    Failed,
    /// The child was interrupted with its parent.
    Interrupted,
}

/// Typed event emitted on the single `blackwall://event` channel.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Instruction provenance and safe discovery warnings, without file contents.
    InstructionsLoaded {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        sources: Vec<String>,
        warnings: Vec<String>,
    },
    /// Latest estimated request budget, distinct from endpoint-reported usage.
    ContextReport {
        #[serde(rename = "requestId")]
        request_id: String,
        report: crate::context::ContextReport,
    },
    /// Original history and checkpoints; no approval state is serialized.
    ContextUpdated {
        #[serde(rename = "requestId")]
        request_id: String,
        state: crate::context::ContextState,
    },
    /// Incremental assistant text from a streaming model response.
    AssistantDelta {
        /// Identifier of the request producing this delta.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Newly generated text; clients append it exactly once.
        delta: String,
    },
    /// A tool invocation proposed by the model.
    ToolCall {
        /// Identifier of the request producing this call.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Endpoint-generated identifier for the invocation.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Registered tool name.
        name: String,
        /// JSON-encoded tool arguments.
        arguments: String,
    },
    /// Structured output returned by a completed tool invocation.
    ToolResult {
        /// Identifier of the request producing this result.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Identifier matching the preceding tool call.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Human-readable or JSON tool output.
        output: String,
        /// Whether the tool completed successfully.
        success: bool,
        /// Whether command output over the cap was trimmed to its start and end.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// A gated operation waiting for a user decision.
    ApprovalRequest {
        /// Identifier of the request waiting for approval.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Unique identifier used when resolving the prompt.
        #[serde(rename = "approvalId")]
        approval_id: String,
        /// Security category of the operation.
        kind: ApprovalKind,
        /// Exact command, file change, host, or delegated capability.
        detail: String,
        /// The MCP tool, for approvals that can be saved as an owner rule.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<ApprovalTool>,
        /// Whether "allow identical action this run" is offered. False for a tool call the
        /// model wrote as text, which always asks.
        #[serde(default = "offered", skip_serializing_if = "is_offered")]
        remember: bool,
        /// The action hands data to another program that may send it off this device, such as
        /// a subagent running on an owner command tool. Shown as "Sends data off this device".
        #[serde(
            default,
            rename = "offDevice",
            skip_serializing_if = "std::ops::Not::not"
        )]
        off_device: bool,
    },
    /// Lifecycle update from a child agent.
    SubagentStatus {
        /// Identifier of the parent request.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Stable identifier of the child agent.
        #[serde(rename = "agentId")]
        agent_id: String,
        /// Stable identifier of the parent agent when nested.
        #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        /// Current child lifecycle state.
        state: SubagentState,
        /// Optional status summary or final result.
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// Token usage the server reported for one model call, sent as soon as the call returns so
    /// a run that later fails or stops still accounts for it.
    ModelUsage {
        /// Identifier of the request that made the call.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Tokens for this call only.
        usage: TokenUsage,
    },
    /// Successful end of a model turn after all deltas.
    TurnComplete {
        /// Identifier of the completed request.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Full assistant message assembled from emitted deltas.
        message: ChatMessage,
        /// Endpoint-specific finish reason.
        #[serde(rename = "finishReason", skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        /// Token usage when supplied by the endpoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        /// The model's reasoning for this turn, shown apart from the answer and never sent back
        /// to the model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        /// How long the model reasoned, in milliseconds.
        #[serde(
            rename = "reasoningMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        reasoning_ms: Option<u64>,
    },
    /// Recoverable or terminal failure associated with a request.
    Error {
        /// Identifier of the failed request.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Stable machine-readable category.
        code: String,
        /// Safe human-readable explanation.
        message: String,
        /// Whether retrying without changing the request may succeed.
        retryable: bool,
    },
    /// Notification that the agent's local memory changed.
    MemoryUpdated {
        /// Identifier of the request that caused the update.
        #[serde(rename = "requestId")]
        request_id: String,
        /// `user` or `agent` memory store.
        store: String,
        /// Stable identifier of the affected entry.
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    /// Notification that a local skill changed.
    SkillUpdated {
        /// Identifier of the request that caused the update.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Skill name used in the prompt index.
        name: String,
        /// `created`, `updated`, or `deleted`.
        action: String,
    },
}

fn offered() -> bool {
    true
}
fn is_offered(value: &bool) -> bool {
    *value
}

impl AgentEvent {
    /// Returns the correlation identifier shared by every event variant.
    pub fn request_id(&self) -> &str {
        match self {
            Self::InstructionsLoaded { request_id, .. }
            | Self::ContextReport { request_id, .. }
            | Self::ContextUpdated { request_id, .. }
            | Self::AssistantDelta { request_id, .. }
            | Self::ToolCall { request_id, .. }
            | Self::ToolResult { request_id, .. }
            | Self::ApprovalRequest { request_id, .. }
            | Self::SubagentStatus { request_id, .. }
            | Self::ModelUsage { request_id, .. }
            | Self::TurnComplete { request_id, .. }
            | Self::Error { request_id, .. }
            | Self::MemoryUpdated { request_id, .. }
            | Self::SkillUpdated { request_id, .. } => request_id,
        }
    }
}

/// Protocol validation failure detected before I/O.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ProtocolError {
    /// Invalid model context, output reservation, or agent limits.
    #[error("Invalid context budget, output reserve, or agent limits.")]
    InvalidContextBudget,
    /// No request correlation identifier was provided.
    #[error("requestId must not be empty")]
    MissingRequestId,
    /// No model was selected.
    #[error("model must not be empty")]
    MissingModel,
    /// A turn was attempted without transcript messages.
    #[error("messages must contain at least one item")]
    MissingMessages,
    /// An attachment had no stable client identifier.
    #[error("attachment id must not be empty")]
    MissingAttachmentId,
    /// An attachment had no display name.
    #[error("attachment name must not be empty")]
    MissingAttachmentName,
    /// An attachment had no media type.
    #[error("attachment mimeType must not be empty")]
    MissingAttachmentMediaType,
    /// An attachment exceeded the IPC safety limit.
    #[error("attachment {name:?} is {size_bytes} bytes; the maximum is {maximum_bytes} bytes")]
    AttachmentTooLarge {
        /// Attachment display name.
        name: String,
        /// Reported source size.
        size_bytes: u64,
        /// Configured maximum source size.
        maximum_bytes: u64,
    },
    /// One message contained more files than the protocol permits.
    #[error("message has {count} attachments; the maximum is {maximum}")]
    TooManyAttachments {
        /// Number of supplied attachments.
        count: usize,
        /// Maximum supported attachment count.
        maximum: usize,
    },
    /// One message exceeded the combined source-size limit.
    #[error("message attachments total {total_bytes} bytes; the maximum is {maximum_bytes} bytes")]
    AttachmentsTooLarge {
        /// Combined reported source size.
        total_bytes: u64,
        /// Maximum supported combined source size.
        maximum_bytes: u64,
    },
    /// Temperature was outside the OpenAI-compatible range.
    #[error("temperature {0} is outside the supported range 0.0..=2.0")]
    InvalidTemperature(f32),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sample_request() -> ChatRequest {
        ChatRequest {
            request_id: "request-7".to_owned(),
            endpoint: Some("http://127.0.0.1:1234/v1".to_owned()),
            model: "local-model".to_owned(),
            messages: vec![ChatMessage::new(
                MessageRole::User,
                "What is in this image?",
            )],
            attachments: vec![Attachment {
                id: "attachment-2".to_owned(),
                name: "desk.png".to_owned(),
                mime_type: "image/png".to_owned(),
                size_bytes: 42,
                data_url: Some("data:image/png;base64,AA==".to_owned()),
                text_content: None,
            }],
            temperature: Some(0.2),
            max_tokens: Some(512),
            context_budget: Default::default(),
            context_state: None,
            compact: false,
            text_tool_calls: false,
        }
    }

    #[test]
    fn text_tool_calls_are_off_unless_a_request_asks() {
        let request: ChatRequest =
            serde_json::from_value(serde_json::json!({"requestId":"r","model":"m","messages":[]}))
                .unwrap();
        assert!(!request.text_tool_calls);
        assert!(serde_json::to_value(&request)
            .unwrap()
            .get("textToolCalls")
            .is_none());
        let on: ChatRequest = serde_json::from_value(
            serde_json::json!({"requestId":"r","model":"m","messages":[],"textToolCalls":true}),
        )
        .unwrap();
        assert!(on.text_tool_calls);
    }

    #[test]
    fn request_round_trips_with_camel_case_fields() {
        let request = sample_request();
        let value = serde_json::to_value(&request).unwrap();

        assert_eq!(value["requestId"], "request-7");
        assert_eq!(value["attachments"][0]["mimeType"], "image/png");
        assert_eq!(value["attachments"][0]["sizeBytes"], 42);
        assert_eq!(
            value["attachments"][0]["dataUrl"],
            "data:image/png;base64,AA=="
        );

        let decoded: ChatRequest = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, request);
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn ui_message_attachments_deserialize_without_transport_fields() {
        let value = serde_json::json!({
            "requestId": "request-ui",
            "model": "llava",
            "messages": [{
                "role": "user",
                "content": "Review this file",
                "attachments": [{
                    "id": "attachment-ui",
                    "name": "notes.txt",
                    "mimeType": "text/plain",
                    "sizeBytes": 12,
                    "kind": "text",
                    "textContent": "local-only text"
                }]
            }]
        });

        let request: ChatRequest = serde_json::from_value(value).unwrap();
        let attachment = &request.messages[0].attachments[0];
        assert_eq!(attachment.text_content.as_deref(), Some("local-only text"));
        assert!(request.validate().is_ok());
    }

    #[test]
    fn every_event_variant_round_trips_and_retains_request_id() {
        let message = ChatMessage::new(MessageRole::Assistant, "Done");
        let events = vec![
            AgentEvent::InstructionsLoaded {
                request_id: "r1".into(),
                agent_id: None,
                sources: vec!["AGENTS.md".into()],
                warnings: vec!["Nested guidance was skipped.".into()],
            },
            AgentEvent::ContextReport {
                request_id: "r1".to_owned(),
                report: crate::context::ContextBudget::default().report(&[], &[], None),
            },
            AgentEvent::ContextUpdated {
                request_id: "r1".to_owned(),
                state: crate::context::ContextState::default(),
            },
            AgentEvent::AssistantDelta {
                request_id: "r1".to_owned(),
                delta: "Hello".to_owned(),
            },
            AgentEvent::ToolCall {
                request_id: "r1".to_owned(),
                tool_call_id: "tc1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"README.md\"}".to_owned(),
            },
            AgentEvent::ToolResult {
                request_id: "r1".to_owned(),
                tool_call_id: "tc1".to_owned(),
                output: "# Blackwall".to_owned(),
                success: true,
                truncated: false,
            },
            AgentEvent::ApprovalRequest {
                request_id: "r1".to_owned(),
                approval_id: "a1".to_owned(),
                kind: ApprovalKind::Exec,
                detail: "cargo test".to_owned(),
                tool: None,
                remember: true,
                off_device: false,
            },
            AgentEvent::ApprovalRequest {
                request_id: "r1".to_owned(),
                approval_id: "a3".to_owned(),
                kind: ApprovalKind::File,
                detail: "write_file written as text".to_owned(),
                tool: None,
                remember: false,
                off_device: false,
            },
            AgentEvent::ApprovalRequest {
                request_id: "r1".to_owned(),
                approval_id: "a2".to_owned(),
                kind: ApprovalKind::Exec,
                detail: "MCP server: docs".to_owned(),
                tool: Some(ApprovalTool {
                    server: "docs".to_owned(),
                    tool: "docs__search".to_owned(),
                }),
                remember: true,
                off_device: false,
            },
            AgentEvent::SubagentStatus {
                request_id: "r1".to_owned(),
                agent_id: "child-1".to_owned(),
                parent_id: Some("main".to_owned()),
                state: SubagentState::Running,
                summary: None,
            },
            AgentEvent::TurnComplete {
                request_id: "r1".to_owned(),
                message,
                finish_reason: Some("stop".to_owned()),
                usage: Some(TokenUsage {
                    prompt_tokens: 4,
                    completion_tokens: 2,
                    total_tokens: 6,
                    ..Default::default()
                }),
                reasoning: Some("Checked the plan.".to_owned()),
                reasoning_ms: Some(1500),
            },
            AgentEvent::Error {
                request_id: "r1".to_owned(),
                code: "model_unavailable".to_owned(),
                message: "Ollama is not running".to_owned(),
                retryable: true,
            },
            AgentEvent::MemoryUpdated {
                request_id: "r1".to_owned(),
                store: "agent".to_owned(),
                entry_id: "m1".to_owned(),
            },
            AgentEvent::SkillUpdated {
                request_id: "r1".to_owned(),
                name: "repo-orientation".to_owned(),
                action: "updated".to_owned(),
            },
        ];

        for event in events {
            let encoded = serde_json::to_string(&event).unwrap();
            let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, event);
            assert_eq!(decoded.request_id(), "r1");
        }
    }

    #[test]
    fn validation_rejects_unsafe_attachment_size() {
        let mut request = sample_request();
        request.attachments[0].size_bytes = MAX_ATTACHMENT_BYTES + 1;

        assert!(matches!(
            request.validate(),
            Err(ProtocolError::AttachmentTooLarge { .. })
        ));
    }
    #[test]
    fn token_usage_reads_cached_and_reasoning_details() {
        let usage: TokenUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 40,
            "total_tokens": 140,
            "prompt_tokens_details": {"cached_tokens": 60},
            "completion_tokens_details": {"reasoning_tokens": 15}
        }))
        .unwrap();
        assert_eq!(
            (usage.cached_input_tokens, usage.reasoning_tokens),
            (60, 15)
        );
        // Old saved usage and servers without details load unchanged.
        let plain: TokenUsage = serde_json::from_value(serde_json::json!({
            "promptTokens": 5, "completionTokens": 2, "totalTokens": 7,
            "prompt_tokens_details": null
        }))
        .unwrap();
        assert_eq!(plain.cached_input_tokens, 0);
        assert_eq!(
            serde_json::to_value(plain).unwrap(),
            serde_json::json!({"promptTokens": 5, "completionTokens": 2, "totalTokens": 7})
        );
        // Details never exceed their totals, and stored usage round-trips.
        let clamped: TokenUsage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 10, "prompt_tokens_details": {"cached_tokens": 99}
        }))
        .unwrap();
        assert_eq!(clamped.cached_input_tokens, 10);
        let round: TokenUsage =
            serde_json::from_value(serde_json::to_value(usage).unwrap()).unwrap();
        assert_eq!(round, usage);
    }
}
