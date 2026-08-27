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
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// Tokens consumed by the prompt.
    #[serde(default, alias = "prompt_tokens")]
    pub prompt_tokens: u64,
    /// Tokens generated by the model.
    #[serde(default, alias = "completion_tokens")]
    pub completion_tokens: u64,
    /// Total tokens consumed by this request.
    #[serde(default, alias = "total_tokens")]
    pub total_tokens: u64,
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

impl AgentEvent {
    /// Returns the correlation identifier shared by every event variant.
    pub fn request_id(&self) -> &str {
        match self {
            Self::AssistantDelta { request_id, .. }
            | Self::ToolCall { request_id, .. }
            | Self::ToolResult { request_id, .. }
            | Self::ApprovalRequest { request_id, .. }
            | Self::SubagentStatus { request_id, .. }
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
        }
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
            },
            AgentEvent::ApprovalRequest {
                request_id: "r1".to_owned(),
                approval_id: "a1".to_owned(),
                kind: ApprovalKind::Exec,
                detail: "cargo test".to_owned(),
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
                }),
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
}
