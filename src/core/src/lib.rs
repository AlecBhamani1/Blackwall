//! Framework-free types and agent primitives shared by every Blackwall client.
//!
//! This crate deliberately does not depend on Tauri (or any other presentation
//! framework). Desktop, CLI, and future remote clients communicate through the
//! serde protocol exported here.

pub mod connection;
pub mod jobs;
pub mod memory;
pub mod protocol;
pub mod share;
pub mod storage;

pub use protocol::{
    AgentEvent, ApprovalDecision, ApprovalKind, Attachment, ChatMessage, ChatRequest, ChatResponse,
    MessageRole, ModelCatalog, ModelInfo, ProtocolError, ResolveApprovalRequest, StreamStarted,
    SubagentState, TokenUsage,
};

pub mod agent;
pub mod approvals;
pub mod model;
pub mod tools;

pub mod web;

pub mod subagents;

pub mod skills;

pub mod auth;

pub mod pairing;
