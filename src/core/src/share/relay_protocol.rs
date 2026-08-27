//! Versioned messages exchanged between a Blackwall host and a hosted relay.

use serde::{Deserialize, Serialize};

/// Largest guest chat body accepted by the relay and desktop host.
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// Largest streamed model response accepted for one guest request.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// First message sent by a desktop host when it attaches to a relay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayRegistration {
    /// Protocol version. Version 1 is the first hosted-relay contract.
    pub protocol_version: u16,
    /// Cryptographically random, URL-safe identifier for this share.
    pub session_id: String,
    /// Secret used to authenticate reconnects for this session.
    pub host_key: String,
    /// Optional deployment-wide registration credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_token: Option<String>,
    /// Model identifier shown to guests and forced onto every request.
    pub model: String,
    /// Salt for the guest invite-key digest, encoded with base64url.
    pub guest_key_salt: String,
    /// Salted guest invite-key digest, encoded with base64url.
    pub guest_key_hash: String,
    /// Invite expiry as milliseconds since the Unix epoch.
    pub expires_at_ms: u64,
}

/// Messages sent from the desktop host to the hosted relay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToRelay {
    /// Registers a new share or reattaches its authenticated host.
    Register { registration: RelayRegistration },
    /// Begins a response to one relayed guest request.
    ResponseStart {
        request_id: String,
        status: u16,
        content_type: String,
    },
    /// Carries one base64url-encoded response chunk.
    ResponseChunk { request_id: String, data: String },
    /// Completes a relayed response successfully.
    ResponseEnd { request_id: String },
    /// Aborts a response whose headers may already have reached the guest.
    ResponseAbort { request_id: String, message: String },
}

/// Messages sent from the hosted relay to the desktop host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayToHost {
    /// Confirms that the relay published the registered session.
    Registered,
    /// Delivers one base64url-encoded OpenAI-compatible request body.
    ChatRequest { request_id: String, body: String },
    /// Reports one authenticated guest API request for host-side status.
    RequestAccepted,
    /// Rejects registration or reports a terminal protocol problem.
    Error { message: String },
}
