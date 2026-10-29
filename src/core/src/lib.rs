//! Framework-free types and agent primitives shared by every Blackwall client.
//!
//! This crate deliberately does not depend on Tauri (or any other presentation
//! framework). Desktop, CLI, and future remote clients communicate through the
//! serde protocol exported here.

pub mod connection;
pub mod context;
pub mod jobs;
pub mod memory;
pub mod protocol;
pub mod review;
pub mod share;
pub mod storage;

pub use protocol::{
    AgentEvent, ApprovalDecision, ApprovalKind, Attachment, ChatMessage, ChatRequest, ChatResponse,
    MessageRole, ModelCatalog, ModelInfo, ProtocolError, ResolveApprovalRequest, StreamStarted,
    SubagentState, TokenUsage,
};

pub mod agent;
pub mod approvals;
pub mod command_tools;
pub mod instructions;
pub mod mcp;
pub mod model;
pub mod policy;
pub mod project;
pub mod secrets;
pub mod telemetry;
pub mod tools;
pub mod trace;
pub mod usage;

pub mod web;

pub mod subagents;

pub mod skill_import;
pub mod skill_scan;
pub mod skill_suggest;
pub mod skills;

pub mod auth;

pub mod pairing;

pub mod sandbox;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod test_http;
