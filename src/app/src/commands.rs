use std::{borrow::Cow, fmt, mem, str, time::Duration};

use blackwall_core::protocol::{
    AgentEvent, Attachment, ChatMessage, ChatRequest, ChatResponse, MessageRole, ModelCatalog,
    ModelInfo, ProtocolError, StreamStarted, TokenUsage, DEFAULT_MODEL_ENDPOINT,
};
use blackwall_core::share::{ShareError, ShareHub, ShareStatus, StartShareRequest};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use thiserror::Error;

const EVENT_CHANNEL: &str = "blackwall://event";
const MODEL_ENDPOINT_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_MODEL_ENDPOINT";
const OLLAMA_HOST_ENVIRONMENT_VARIABLE: &str = "OLLAMA_HOST";
const API_KEY_ENVIRONMENT_VARIABLE: &str = "BLACKWALL_MODEL_API_KEY";
const MAX_ERROR_BODY_BYTES: usize = 1_000;
const MAX_MODEL_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_MODEL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;
const MODEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_CHAT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MODEL_STREAM_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Process-wide dependencies shared by Tauri commands.
pub(crate) struct AppState {
    client: Client,
    default_endpoint: String,
    api_key: Option<String>,
    share_hub: ShareHub,
}

impl AppState {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(MODEL_CONNECT_TIMEOUT)
                .user_agent(concat!("blackwall/", env!("CARGO_PKG_VERSION")))
                .build()?,
            default_endpoint: environment_value(MODEL_ENDPOINT_ENVIRONMENT_VARIABLE)
                .or_else(|| environment_value(OLLAMA_HOST_ENVIRONMENT_VARIABLE))
                .unwrap_or_else(|| DEFAULT_MODEL_ENDPOINT.to_owned()),
            api_key: std::env::var(API_KEY_ENVIRONMENT_VARIABLE)
                .ok()
                .filter(|value| !value.trim().is_empty()),
            share_hub: ShareHub::new(),
        })
    }
}

/// Error shape serialized across the Tauri IPC boundary.
#[derive(Clone, Debug, Error, Serialize)]
#[error("{message}")]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandError {
    code: &'static str,
    message: String,
    retryable: bool,
}

impl CommandError {
    fn from_bridge(error: BridgeError) -> Self {
        let retryable = match &error {
            BridgeError::Request(source) => source.is_connect() || source.is_timeout(),
            BridgeError::HttpStatus { status, .. } => {
                *status == StatusCode::REQUEST_TIMEOUT
                    || *status == StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error()
            }
            _ => false,
        };
        let code = match &error {
            BridgeError::Protocol(_)
            | BridgeError::InvalidEndpoint { .. }
            | BridgeError::UnsupportedEndpointScheme(_)
            | BridgeError::EndpointCredentialsNotAllowed
            | BridgeError::AttachmentsRequireUserMessage => "invalid_request",
            BridgeError::Request(source) if source.is_timeout() => "model_timeout",
            BridgeError::Request(_) => "model_unavailable",
            BridgeError::HttpStatus { .. } => "model_http_error",
            BridgeError::ModelError(_) => "model_error",
            BridgeError::Decode(_) | BridgeError::InvalidEventEncoding(_) => {
                "invalid_model_response"
            }
            BridgeError::ResponseTooLarge { .. } => "model_response_too_large",
            BridgeError::EmptyModelResponse => "empty_model_response",
            BridgeError::Emit(_) => "event_bridge_error",
        };

        Self {
            code,
            message: error.to_string(),
            retryable,
        }
    }
}

impl From<BridgeError> for CommandError {
    fn from(error: BridgeError) -> Self {
        Self::from_bridge(error)
    }
}

impl From<ProtocolError> for CommandError {
    fn from(error: ProtocolError) -> Self {
        Self::from_bridge(BridgeError::Protocol(error))
    }
}

impl From<ShareError> for CommandError {
    fn from(error: ShareError) -> Self {
        Self {
            code: "sharing_error",
            message: error.to_string(),
            retryable: false,
        }
    }
}

#[derive(Debug, Error)]
enum BridgeError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("invalid model endpoint {endpoint:?}: {reason}")]
    InvalidEndpoint { endpoint: String, reason: String },
    #[error("model endpoint must use http or https, not {0:?}")]
    UnsupportedEndpointScheme(String),
    #[error("model endpoint credentials must be supplied outside the URL")]
    EndpointCredentialsNotAllowed,
    #[error("attachments require at least one user message")]
    AttachmentsRequireUserMessage,
    #[error("could not reach the configured model endpoint: {0}")]
    Request(#[source] reqwest::Error),
    #[error("model endpoint returned HTTP {status}: {body}")]
    HttpStatus { status: StatusCode, body: String },
    #[error("model endpoint rejected the request: {0}")]
    ModelError(String),
    #[error("model endpoint returned invalid JSON: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("model stream contained invalid UTF-8: {0}")]
    InvalidEventEncoding(#[source] str::Utf8Error),
    #[error("model response exceeded the {maximum_bytes}-byte safety limit")]
    ResponseTooLarge { maximum_bytes: usize },
    #[error("model endpoint returned no assistant choice")]
    EmptyModelResponse,
    #[error("could not emit a desktop event: {0}")]
    Emit(#[source] tauri::Error),
}

/// Returns the normalized endpoint selected from the process configuration.
#[tauri::command]
pub(crate) fn model_endpoint(state: State<'_, AppState>) -> Result<String, CommandError> {
    normalize_endpoint(&state.default_endpoint).map_err(CommandError::from)
}

/// Lists chat models advertised by the configured OpenAI-compatible endpoint.
#[tauri::command]
pub(crate) async fn discover_models(
    endpoint: Option<String>,
    state: State<'_, AppState>,
) -> Result<ModelCatalog, CommandError> {
    let endpoint = normalize_endpoint(
        endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(state.default_endpoint.as_str()),
    )
    .map_err(CommandError::from)?;
    let response = authorized_request(
        state
            .client
            .get(format!("{endpoint}/models"))
            .timeout(MODEL_DISCOVERY_TIMEOUT),
        state.api_key.as_deref(),
    )
    .send()
    .await
    .map_err(BridgeError::Request)
    .map_err(CommandError::from)?;
    let response = require_success(response)
        .await
        .map_err(CommandError::from)?;
    let mut payload: ApiModelCatalog = read_json_bounded(response, MAX_MODEL_CATALOG_BYTES)
        .await
        .map_err(CommandError::from)?;

    if payload.data.is_empty() {
        payload.data = payload.models;
    }

    Ok(ModelCatalog {
        endpoint,
        models: payload
            .data
            .into_iter()
            .filter(|model| !model.id.trim().is_empty())
            .map(|model| ModelInfo {
                id: model.id,
                name: model.name,
                owned_by: model.owned_by,
            })
            .collect(),
    })
}

/// Runs a complete non-streaming chat request.
#[tauri::command]
pub(crate) async fn chat(
    request: ChatRequest,
    state: State<'_, AppState>,
) -> Result<ChatResponse, CommandError> {
    request.validate()?;
    validate_attachment_target(&request).map_err(CommandError::from)?;
    complete_chat(
        &state.client,
        state.api_key.as_deref(),
        &state.default_endpoint,
        request,
    )
    .await
    .map_err(CommandError::from)
}

/// Runs a model stream, emitting typed events until completion.
#[tauri::command]
pub(crate) async fn stream_chat(
    request: ChatRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<StreamStarted, CommandError> {
    let request_id = request.request_id.clone();
    let setup_result = request
        .validate()
        .map_err(BridgeError::from)
        .and_then(|()| validate_attachment_target(&request))
        .and_then(|()| selected_endpoint(&request, &state.default_endpoint).map(|_| ()));
    if let Err(error) = setup_result {
        return Err(emit_command_error(
            &app,
            request_id,
            CommandError::from(error),
        ));
    }

    let stream_result = stream_chat_events(
        &state.client,
        state.api_key.as_deref(),
        &state.default_endpoint,
        &app,
        request,
    )
    .await;
    if let Err(error) = stream_result {
        return Err(emit_command_error(
            &app,
            request_id,
            CommandError::from(error),
        ));
    }

    Ok(StreamStarted { request_id })
}

/// Starts a temporary browser invite through the selected hosted relay.
#[tauri::command]
pub(crate) async fn start_share(
    request: StartShareRequest,
    state: State<'_, AppState>,
) -> Result<ShareStatus, CommandError> {
    state
        .share_hub
        .start(request)
        .await
        .map_err(CommandError::from)
}

/// Returns non-secret guest listener metadata.
#[tauri::command]
pub(crate) async fn share_status(state: State<'_, AppState>) -> Result<ShareStatus, CommandError> {
    Ok(state.share_hub.status().await)
}

/// Revokes the active invite and closes its network listener.
#[tauri::command]
pub(crate) async fn stop_share(state: State<'_, AppState>) -> Result<ShareStatus, CommandError> {
    Ok(state.share_hub.stop().await)
}

async fn complete_chat(
    client: &Client,
    api_key: Option<&str>,
    default_endpoint: &str,
    request: ChatRequest,
) -> Result<ChatResponse, BridgeError> {
    let endpoint = selected_endpoint(&request, default_endpoint)?;
    let messages = build_api_messages(&request)?;
    let payload = ApiCompletionRequest {
        model: &request.model,
        messages,
        stream: false,
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        stream_options: None,
    };
    let response = authorized_request(
        client
            .post(format!("{endpoint}/chat/completions"))
            .timeout(MODEL_CHAT_TIMEOUT)
            .json(&payload),
        api_key,
    )
    .send()
    .await
    .map_err(BridgeError::Request)?;
    let response = require_success(response).await?;
    let payload: ApiCompletionResponse =
        read_json_bounded(response, MAX_MODEL_RESPONSE_BYTES).await?;
    if let Some(error) = payload.error {
        return Err(BridgeError::ModelError(error.into_message()));
    }
    let choice = payload
        .choices
        .into_iter()
        .next()
        .ok_or(BridgeError::EmptyModelResponse)?;

    Ok(ChatResponse {
        request_id: request.request_id,
        model: payload.model.unwrap_or(request.model),
        message: ChatMessage::new(
            choice.message.role.unwrap_or(MessageRole::Assistant),
            choice.message.content.unwrap_or_default(),
        ),
        finish_reason: choice.finish_reason,
        usage: payload.usage,
    })
}

async fn stream_chat_events(
    client: &Client,
    api_key: Option<&str>,
    default_endpoint: &str,
    app: &AppHandle,
    request: ChatRequest,
) -> Result<(), BridgeError> {
    let endpoint = selected_endpoint(&request, default_endpoint)?;
    let messages = build_api_messages(&request)?;
    let payload = ApiCompletionRequest {
        model: &request.model,
        messages,
        stream: true,
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
    };
    let response = authorized_request(
        client
            .post(format!("{endpoint}/chat/completions"))
            .timeout(MODEL_STREAM_TIMEOUT)
            .json(&payload),
        api_key,
    )
    .send()
    .await
    .map_err(BridgeError::Request)?;
    let response = require_success(response).await?;
    let mut bytes = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut content = String::new();
    let mut finish_reason = None;
    let mut usage = None;
    let mut done = false;
    let mut received_bytes = 0_usize;

    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(BridgeError::Request)?;
        received_bytes = received_bytes.saturating_add(chunk.len());
        if received_bytes > MAX_MODEL_RESPONSE_BYTES {
            return Err(BridgeError::ResponseTooLarge {
                maximum_bytes: MAX_MODEL_RESPONSE_BYTES,
            });
        }
        decoder.push(&chunk);
        while let Some(frame) = decoder.next_frame() {
            if consume_stream_frame(
                app,
                &request.request_id,
                &frame,
                &mut content,
                &mut finish_reason,
                &mut usage,
            )? {
                done = true;
                break;
            }
        }
        if decoder.len() > MAX_SSE_FRAME_BYTES {
            return Err(BridgeError::ResponseTooLarge {
                maximum_bytes: MAX_SSE_FRAME_BYTES,
            });
        }
        if done {
            break;
        }
    }

    if let Some(frame) = decoder.take_remainder().filter(|_| !done) {
        let _done = consume_stream_frame(
            app,
            &request.request_id,
            &frame,
            &mut content,
            &mut finish_reason,
            &mut usage,
        )?;
    }

    emit_event(
        app,
        &AgentEvent::TurnComplete {
            request_id: request.request_id,
            message: ChatMessage::new(MessageRole::Assistant, content),
            finish_reason,
            usage,
        },
    )
}

fn consume_stream_frame(
    app: &AppHandle,
    request_id: &str,
    frame: &[u8],
    content: &mut String,
    finish_reason: &mut Option<String>,
    usage: &mut Option<TokenUsage>,
) -> Result<bool, BridgeError> {
    let Some(data) = sse_data(frame)? else {
        return Ok(false);
    };
    if data.trim() == "[DONE]" {
        return Ok(true);
    }

    let chunk: ApiCompletionChunk = serde_json::from_str(&data).map_err(BridgeError::Decode)?;
    if let Some(error) = chunk.error {
        return Err(BridgeError::ModelError(error.into_message()));
    }
    if let Some(chunk_usage) = chunk.usage {
        *usage = Some(chunk_usage);
    }
    for choice in chunk.choices {
        if let Some(reason) = choice.finish_reason {
            *finish_reason = Some(reason);
        }
        if let Some(delta) = choice.delta.content.filter(|delta| !delta.is_empty()) {
            content.push_str(&delta);
            emit_event(
                app,
                &AgentEvent::AssistantDelta {
                    request_id: request_id.to_owned(),
                    delta,
                },
            )?;
        }
    }
    Ok(false)
}

fn emit_event(app: &AppHandle, event: &AgentEvent) -> Result<(), BridgeError> {
    app.emit(EVENT_CHANNEL, event).map_err(BridgeError::Emit)
}

fn emit_command_error(app: &AppHandle, request_id: String, error: CommandError) -> CommandError {
    let event = AgentEvent::Error {
        request_id,
        code: error.code.to_owned(),
        message: error.message.clone(),
        retryable: error.retryable,
    };
    let _emit_result = app.emit(EVENT_CHANNEL, event);
    error
}

fn selected_endpoint(request: &ChatRequest, default_endpoint: &str) -> Result<String, BridgeError> {
    normalize_endpoint(
        request
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(default_endpoint),
    )
}

fn normalize_endpoint(endpoint: &str) -> Result<String, BridgeError> {
    let endpoint = endpoint.trim();
    let raw = if endpoint.contains("://") {
        Cow::Borrowed(endpoint)
    } else {
        Cow::Owned(format!("http://{endpoint}"))
    };
    let mut parsed = Url::parse(&raw).map_err(|error| BridgeError::InvalidEndpoint {
        endpoint: endpoint.to_owned(),
        reason: error.to_string(),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(BridgeError::UnsupportedEndpointScheme(
            parsed.scheme().to_owned(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BridgeError::EndpointCredentialsNotAllowed);
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    if parsed.path().is_empty() || parsed.path() == "/" {
        parsed.set_path("/v1");
    }
    let normalized = parsed.as_str().trim_end_matches('/').to_owned();
    Ok(normalized)
}

fn environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn validate_attachment_target(request: &ChatRequest) -> Result<(), BridgeError> {
    if !request.attachments.is_empty()
        && !request
            .messages
            .iter()
            .any(|message| message.role == MessageRole::User)
    {
        return Err(BridgeError::AttachmentsRequireUserMessage);
    }
    if request
        .messages
        .iter()
        .any(|message| !message.attachments.is_empty() && message.role != MessageRole::User)
    {
        return Err(BridgeError::AttachmentsRequireUserMessage);
    }
    Ok(())
}

fn authorized_request(
    request: reqwest::RequestBuilder,
    api_key: Option<&str>,
) -> reqwest::RequestBuilder {
    match api_key {
        Some(api_key) => request.bearer_auth(api_key),
        None => request,
    }
}

async fn require_success(response: reqwest::Response) -> Result<reqwest::Response, BridgeError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = read_error_body(response).await;
    Err(BridgeError::HttpStatus { status, body })
}

async fn read_json_bounded<T>(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<T, BridgeError>
where
    T: DeserializeOwned,
{
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err(BridgeError::ResponseTooLarge { maximum_bytes });
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BridgeError::Request)?;
        if body.len().saturating_add(chunk.len()) > maximum_bytes {
            return Err(BridgeError::ResponseTooLarge { maximum_bytes });
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(BridgeError::Decode)
}

async fn read_error_body(response: reqwest::Response) -> String {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return format!("could not read error response: {error}"),
        };
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
        if body.len() == MAX_ERROR_BODY_BYTES {
            truncated = true;
            break;
        }
    }
    let body = String::from_utf8_lossy(&body);
    let body = body.trim();
    if truncated {
        format!("{body}…")
    } else {
        body.to_owned()
    }
}

fn build_api_messages(request: &ChatRequest) -> Result<Vec<ApiOutgoingMessage>, BridgeError> {
    let attachment_target = request
        .messages
        .iter()
        .rposition(|message| message.role == MessageRole::User);
    if !request.attachments.is_empty() && attachment_target.is_none() {
        return Err(BridgeError::AttachmentsRequireUserMessage);
    }

    Ok(request
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let mut attachments = message.attachments.iter().collect::<Vec<_>>();
            if Some(index) == attachment_target {
                attachments.extend(request.attachments.iter());
            }
            ApiOutgoingMessage {
                role: message.role,
                content: if attachments.is_empty() {
                    ApiContent::Text(message.content.clone())
                } else {
                    multimodal_content(message, &attachments)
                },
            }
        })
        .collect())
}

fn multimodal_content(message: &ChatMessage, attachments: &[&Attachment]) -> ApiContent {
    let mut parts = Vec::new();
    let mut text = message.content.clone();
    for attachment in attachments {
        match (
            attachment.data_url.as_deref(),
            attachment.text_content.as_deref(),
        ) {
            (Some(data_url), _) if attachment.mime_type.starts_with("image/") => {
                parts.push(ApiContentPart::ImageUrl {
                    image_url: ApiImageUrl {
                        url: data_url.to_owned(),
                    },
                });
            }
            (_, Some(text_content)) => {
                use fmt::Write as _;
                let _format_result = write!(
                    text,
                    "\n\n<attachment name={:?} type={:?}>\n{}\n</attachment>",
                    attachment.name, attachment.mime_type, text_content
                );
            }
            _ => {
                use fmt::Write as _;
                let _format_result = write!(
                    text,
                    "\n\nAttached file: {} ({}, {} bytes).",
                    attachment.name, attachment.mime_type, attachment.size_bytes
                );
            }
        }
    }
    if !text.is_empty() {
        parts.insert(0, ApiContentPart::Text { text });
    }
    ApiContent::Parts(parts)
}

fn sse_data(frame: &[u8]) -> Result<Option<String>, BridgeError> {
    let frame = str::from_utf8(frame).map_err(BridgeError::InvalidEventEncoding)?;
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }
    if data_lines.is_empty() {
        let raw = frame.trim();
        if raw.starts_with('{') || raw == "[DONE]" {
            return Ok(Some(raw.to_owned()));
        }
        return Ok(None);
    }
    Ok(Some(data_lines.join("\n")))
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    fn len(&self) -> usize {
        self.buffer.len()
    }

    fn next_frame(&mut self) -> Option<Vec<u8>> {
        let (position, separator_length) = frame_separator(&self.buffer)?;
        let mut frame = mem::take(&mut self.buffer);
        self.buffer = frame.split_off(position + separator_length);
        frame.truncate(position);
        Some(frame)
    }

    fn take_remainder(&mut self) -> Option<Vec<u8>> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            self.buffer.clear();
            None
        } else {
            Some(mem::take(&mut self.buffer))
        }
    }
}

fn frame_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2));
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(separator), None) | (None, Some(separator)) => Some(separator),
        (None, None) => None,
    }
}

#[derive(Deserialize)]
struct ApiModelCatalog {
    #[serde(default)]
    data: Vec<ApiModel>,
    #[serde(default)]
    models: Vec<ApiModel>,
}

#[derive(Deserialize)]
struct ApiModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    owned_by: Option<String>,
}

#[derive(Serialize)]
struct ApiCompletionRequest<'a> {
    model: &'a str,
    messages: Vec<ApiOutgoingMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ApiOutgoingMessage {
    role: MessageRole,
    content: ApiContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ApiContent {
    Text(String),
    Parts(Vec<ApiContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiContentPart {
    Text { text: String },
    ImageUrl { image_url: ApiImageUrl },
}

#[derive(Serialize)]
struct ApiImageUrl {
    url: String,
}

#[derive(Deserialize)]
struct ApiCompletionResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<ApiCompletionChoice>,
    #[serde(default)]
    usage: Option<TokenUsage>,
    #[serde(default)]
    error: Option<ApiErrorPayload>,
}

#[derive(Deserialize)]
struct ApiCompletionChoice {
    message: ApiIncomingMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiIncomingMessage {
    #[serde(default)]
    role: Option<MessageRole>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct ApiCompletionChunk {
    #[serde(default)]
    choices: Vec<ApiStreamChoice>,
    #[serde(default)]
    usage: Option<TokenUsage>,
    #[serde(default)]
    error: Option<ApiErrorPayload>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ApiErrorPayload {
    Object { message: String },
    Text(String),
}

impl ApiErrorPayload {
    fn into_message(self) -> String {
        match self {
            Self::Object { message } | Self::Text(message) => message,
        }
    }
}

#[derive(Deserialize)]
struct ApiStreamChoice {
    #[serde(default)]
    delta: ApiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct ApiDelta {
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn endpoint_defaults_to_ollama_and_adds_v1_to_origin() {
        assert_eq!(
            normalize_endpoint(DEFAULT_MODEL_ENDPOINT).unwrap(),
            DEFAULT_MODEL_ENDPOINT
        );
        assert_eq!(
            normalize_endpoint("localhost:11434").unwrap(),
            DEFAULT_MODEL_ENDPOINT
        );
        assert_eq!(
            normalize_endpoint("192.168.1.50:11434").unwrap(),
            "http://192.168.1.50:11434/v1"
        );
    }

    #[test]
    fn sse_decoder_preserves_frames_split_across_chunks() {
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: {\"choices\":[{\"delta\":{");
        assert!(decoder.next_frame().is_none());
        decoder.push(b"\"content\":\"hi\"}}]}\n\ndata: [DONE]\r\n\r\n");

        let first = decoder.next_frame().unwrap();
        let second = decoder.next_frame().unwrap();
        assert!(sse_data(&first)
            .unwrap()
            .unwrap()
            .contains("\"content\":\"hi\""));
        assert_eq!(sse_data(&second).unwrap().as_deref(), Some("[DONE]"));
    }

    #[test]
    fn image_attachments_become_openai_content_parts() {
        let request = ChatRequest {
            request_id: "r1".to_owned(),
            endpoint: None,
            model: "vision".to_owned(),
            messages: vec![ChatMessage::new(MessageRole::User, "Describe it")],
            attachments: vec![Attachment {
                id: "a1".to_owned(),
                name: "photo.jpg".to_owned(),
                mime_type: "image/jpeg".to_owned(),
                size_bytes: 12,
                data_url: Some("data:image/jpeg;base64,AA==".to_owned()),
                text_content: None,
            }],
            temperature: None,
            max_tokens: None,
        };

        let messages = build_api_messages(&request).unwrap();
        let value = serde_json::to_value(messages).unwrap();
        assert_eq!(value[0]["content"][0]["type"], "text");
        assert_eq!(value[0]["content"][1]["type"], "image_url");
        assert_eq!(
            value[0]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,AA=="
        );
    }
}
