//! Framework-free types and agent primitives shared by every Blackwall client.
//!
//! This crate deliberately does not depend on Tauri (or any other presentation
//! framework). Desktop, CLI, and future remote clients communicate through the
//! serde protocol exported here.

pub mod protocol;
pub mod share;

pub use protocol::{
    AgentEvent, ApprovalDecision, ApprovalKind, Attachment, ChatMessage, ChatRequest, ChatResponse,
    MessageRole, ModelCatalog, ModelInfo, ProtocolError, ResolveApprovalRequest, StreamStarted,
    SubagentState, TokenUsage,
};
