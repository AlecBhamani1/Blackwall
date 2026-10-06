use std::{borrow::Cow, fmt, time::Duration};

use blackwall_core::protocol::{
    AgentEvent, Attachment, ChatMessage, ChatRequest, ChatResponse, MessageRole, ModelCatalog,
    ModelInfo, ProtocolError, StreamStarted, TokenUsage, DEFAULT_MODEL_ENDPOINT,
};
use blackwall_core::share::{
    manager::{ManagedShare, ShareManager},
    ShareError, ShareStatus, StartShareRequest,
};
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

const MODEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_CHAT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Process-wide dependencies shared by Tauri commands.
pub(crate) struct AppState {
    client: Client,
    default_endpoint: String,
    api_key: Option<String>,
    pub(crate) share_hub: ShareManager,
}

impl AppState {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(MODEL_CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("blackwall/", env!("CARGO_PKG_VERSION")))
                .build()?,
            default_endpoint: environment_value(MODEL_ENDPOINT_ENVIRONMENT_VARIABLE)
                .or_else(|| environment_value(OLLAMA_HOST_ENVIRONMENT_VARIABLE))
                .unwrap_or_else(|| DEFAULT_MODEL_ENDPOINT.to_owned()),
            api_key: std::env::var(API_KEY_ENVIRONMENT_VARIABLE)
                .ok()
                .filter(|value| !value.trim().is_empty()),
            share_hub: ShareManager::new(),
        })
    }
}

// Credentials are resolved in Rust, never returned to the webview.
pub(crate) async fn scoped_api_key(
    state: &AppState,
    endpoint: &str,
) -> Result<Option<String>, CommandError> {
    if let Some(key) = crate::credentials::model_key(endpoint)
        .await
        .map_err(|message| CommandError {
            code: "credential_error",
            message,
            retryable: true,
        })?
    {
        return Ok(Some(key));
    }
    let configured = normalize_endpoint(&state.default_endpoint).map_err(CommandError::from)?;
    Ok(
        if blackwall_core::connection::same_origin(&configured, endpoint) {
            state.api_key.clone()
        } else {
            None
        },
    )
}

async fn require_unlocked(app: &AppHandle) -> Result<(), CommandError> {
    crate::auth::require_unlocked(app)
        .await
        .map_err(|message| CommandError {
            code: "app_locked",
            message,
            retryable: false,
        })
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
            BridgeError::HttpStatus { status, .. } => match *status {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::GONE => {
                    "model_access_denied"
                }
                StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY => "model_unavailable",
                StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "model_timeout",
                StatusCode::TOO_MANY_REQUESTS => "model_busy",
                StatusCode::NOT_FOUND => "model_not_found",
                _ => "model_http_error",
            },
            BridgeError::ModelError(_) => "model_error",
            BridgeError::Decode(_) => "invalid_model_response",
            BridgeError::ResponseTooLarge { .. } => "model_response_too_large",
            BridgeError::EmptyModelResponse => "empty_model_response",
            BridgeError::Emit(_) => "event_bridge_error",
        };

        Self {
            code,
            // Public errors must never include endpoint URLs or upstream response bodies.
            message: match code {
                "model_unavailable" => "The model computer is unavailable. Open and unlock Blackwall on that computer, keep its model running, and reconnect.",
                "model_access_denied" => "Model access was refused or removed. Check your access key, or pair the computer again if its access was removed.",
                "model_timeout" => "The model took too long to respond. Check that its computer is awake and connected, then try again.",
                "model_busy" => "The model is busy. Wait for another request to finish, then try again.",
                "model_not_found" => "The model connection is no longer available. Check that its computer is open, unlocked, and awake. If access was removed, pair again.",
                "invalid_request" => "The request could not be sent. Check the connection settings, model, and attachments.",
                "model_response_too_large" => "The model response was too large. Ask for a shorter response.",
                "empty_model_response" => "The model returned no answer. Try again or choose another model.",
                "invalid_model_response" => "The model returned an unreadable response. Try again or check the model service.",
                _ => "The model could not complete the request. Try again or check the model service.",
            }.into(),
            retryable,
        }
    }
}

impl From<blackwall_core::model::ModelError> for CommandError {
    fn from(error: blackwall_core::model::ModelError) -> Self {
        use blackwall_core::model::ModelError;
        match error {
            ModelError::Http(code) => Self::from_bridge(BridgeError::HttpStatus {
                status: StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
                body: String::new(),
            }),
            ModelError::Network => Self {
                code: "model_unavailable",
                message: error.to_string(),
                retryable: true,
            },
            ModelError::Invalid => Self {
                code: "invalid_model_response",
                message: error.to_string(),
                retryable: false,
            },
            ModelError::Limit => Self {
                code: "context_limit",
                message: error.to_string(),
                retryable: false,
            },
        }
    }
}
impl From<blackwall_core::context::ContextError> for CommandError {
    fn from(error: blackwall_core::context::ContextError) -> Self {
        use blackwall_core::context::ContextError;
        let code = match error {
            ContextError::Model(model) => return model.into(),
            ContextError::Budget => "context_budget_invalid",
            ContextError::Limit => "context_limit",
            ContextError::History => "context_history_invalid",
            ContextError::Summary => "context_summary_invalid",
            ContextError::Nothing => "context_nothing_to_compact",
        };
        Self {
            code,
            message: error.to_string(),
            retryable: false,
        }
    }
}
impl From<blackwall_core::agent::AgentError> for CommandError {
    fn from(error: blackwall_core::agent::AgentError) -> Self {
        use blackwall_core::agent::AgentError;
        match error {
            AgentError::Budget(error) => error.into(),
            AgentError::Model(error) => error.into(),
            AgentError::Iterations => Self {
                code: "agent_iteration_limit",
                message: error.to_string(),
                retryable: false,
            },
            AgentError::Context => Self {
                code: "context_limit",
                message: error.to_string(),
                retryable: false,
            },
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
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ModelCatalog, CommandError> {
    require_unlocked(&app).await?;
    let endpoint = normalize_endpoint(
        endpoint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(state.default_endpoint.as_str()),
    )
    .map_err(CommandError::from)?;
    let api_key = scoped_api_key(&state, &endpoint).await?;
    let response = authorized_request(
        state
            .client
            .get(format!("{endpoint}/models"))
            .timeout(MODEL_DISCOVERY_TIMEOUT),
        api_key.as_deref(),
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

async fn inject_memory(
    app: &AppHandle,
    request: &mut ChatRequest,
    workspace: Option<&str>,
) -> Result<(), CommandError> {
    let workspace = workspace.map(str::to_owned);
    let memory = crate::data::with_store(app, move |store| {
        store.memory_context(
            blackwall_core::memory::MemoryClient::Desktop,
            workspace.as_deref(),
            4000,
        )
    })
    .await
    .map_err(|message| CommandError {
        code: "storage_error",
        message,
        retryable: true,
    })?;
    if !memory.is_empty() {
        request
            .messages
            .insert(0, ChatMessage::new(MessageRole::System, memory));
    }
    Ok(())
}

/// Runs a complete non-streaming chat request.
#[tauri::command]
pub(crate) async fn chat(
    mut request: ChatRequest,
    app: AppHandle,
    state: State<'_, AppState>,
    jobs: State<'_, crate::setup::SetupState>,
) -> Result<ChatResponse, CommandError> {
    require_unlocked(&app).await?;
    request.validate()?;
    let mut job = jobs
        .jobs
        .start(&request.request_id)
        .map_err(|message| CommandError {
            code: "request_stopped",
            message,
            retryable: false,
        })?;
    inject_memory(&app, &mut request, None).await?;
    validate_attachment_target(&request).map_err(CommandError::from)?;
    let selected =
        selected_endpoint(&request, &state.default_endpoint).map_err(CommandError::from)?;
    let api_key = scoped_api_key(&state, &selected).await?;
    tokio::select! {
        biased;
        _ = job.cancelled() => Err(CommandError { code: "request_stopped", message: "Response stopped.".into(), retryable: false }),
        response = complete_chat(&state.client, api_key.as_deref(), &state.default_endpoint, request) => response.map_err(CommandError::from),
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentOptions {
    #[serde(default)]
    enabled: bool,
    workspace: Option<String>,
    #[serde(default)]
    web_enabled: bool,
    session_id: Option<String>,
    source_message_id: Option<String>,
}

/// Runs a model stream, emitting typed events until completion.
#[tauri::command]
pub(crate) async fn stream_chat(
    mut request: ChatRequest,
    app: AppHandle,
    state: State<'_, AppState>,
    jobs: State<'_, crate::setup::SetupState>,
    agent: State<'_, crate::agent_commands::AgentState>,
    options: Option<AgentOptions>,
) -> Result<StreamStarted, CommandError> {
    require_unlocked(&app).await?;
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

    let selected =
        selected_endpoint(&request, &state.default_endpoint).map_err(CommandError::from)?;
    let mut job = jobs
        .jobs
        .start(&request_id)
        .map_err(|message| CommandError {
            code: "request_stopped",
            message,
            retryable: false,
        })?;
    let source = blackwall_core::model::messages(&request);
    let context =
        blackwall_core::context::ContextState::restore(&source, request.context_state.as_ref())
            .map_err(CommandError::from)?;
    let api_key = scoped_api_key(&state, &selected).await?;
    let stopped = || CommandError {
        code: "request_stopped",
        message: "Response stopped.".into(),
        retryable: false,
    };
    let options = options.unwrap_or_default();
    let stream_result = if options.enabled {
        let workspace = options
            .workspace
            .as_deref()
            .ok_or(CommandError {
                code: "workspace_required",
                message: "Choose a project folder before starting an agent task.".into(),
                retryable: false,
            })
            .and_then(|expected| {
                crate::agent_commands::selected_workspace(&agent, expected).map_err(|_| {
                    CommandError {
                        code: "workspace_required",
                        message: "Reopen this conversation to restore its project folder.".into(),
                        retryable: false,
                    }
                })
            })?;
        inject_memory(&app, &mut request, Some(&workspace.path.to_string_lossy())).await?;
        let memory = match (options.session_id, options.source_message_id) {
            (Some(session_id), Some(message_id)) => Some(blackwall_core::memory::MemoryTools {
                directory: crate::identity::data_directory(&app).map_err(|message| {
                    CommandError {
                        code: "storage_error",
                        message,
                        retryable: true,
                    }
                })?,
                client: blackwall_core::memory::MemoryClient::Desktop,
                workspace: Some(workspace.path.to_string_lossy().into_owned()),
                source: blackwall_core::storage::MemorySource {
                    session_id,
                    message_id,
                },
            }),
            _ => None,
        };
        let skill_context = crate::data::with_skills(&app, |store| {
            Ok(blackwall_core::skills::prompt(&store.list()?.0))
        })
        .await
        .map_err(|message| CommandError {
            code: "skills_error",
            message,
            retryable: true,
        })?;
        if !skill_context.is_empty() {
            request
                .messages
                .insert(0, ChatMessage::new(MessageRole::System, skill_context));
        }
        let backend = blackwall_core::model::HttpModel::new(&selected, &request.model, api_key)
            .and_then(|model| model.with_budget(request.context_budget))
            .map(|model| model.with_sampling(request.temperature, request.max_tokens))
            .map_err(CommandError::from)?;
        let event_app = app.clone();
        let runner = blackwall_core::agent::Agent {
            backend: &backend,
            workspace,
            web_enabled: options.web_enabled,
            approvals: agent.approvals.clone(),
            emit: std::sync::Arc::new(move |event| {
                let _ = event_app.emit(EVENT_CHANNEL, event);
            }),
        };
        tokio::select! {
            biased;
            _=job.cancelled()=>Err(stopped()),
            result=runner.run_context_with_memory(&request_id,blackwall_core::context::ContextRun { source: source.clone(), state: context, budget: request.context_budget, instructions: blackwall_core::model::messages(&request).into_iter().take(request.messages.len().saturating_sub(source.len())).collect(), compact: request.compact },memory.as_ref())=>result.map(|_|()).map_err(CommandError::from),
        }
    } else {
        inject_memory(&app, &mut request, None).await?;
        tokio::select! {
            biased;
            _=job.cancelled()=>Err(stopped()),
            result=stream_chat_events(api_key.as_deref(),&state.default_endpoint,&app,request,&source,context)=>result,
        }
    };
    if let Err(error) = stream_result {
        return Err(emit_command_error(&app, request_id, error));
    }

    Ok(StreamStarted { request_id })
}

/// Starts a temporary browser invite through the selected hosted relay.
#[tauri::command]
pub(crate) async fn start_share(
    mut request: StartShareRequest,
    name: Option<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ManagedShare, CommandError> {
    require_unlocked(&app).await?;
    let endpoint = normalize_endpoint(
        request
            .endpoint
            .as_deref()
            .unwrap_or(&state.default_endpoint),
    )?;
    let key = scoped_api_key(&state, &endpoint).await?;
    let relay_origin = request
        .relay_url
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| environment_value("BLACKWALL_RELAY_URL"));
    let supplied_token = request
        .relay_token
        .clone()
        .filter(|value| !value.trim().is_empty());
    if supplied_token.is_none() {
        if let Some(origin) = &relay_origin {
            request.relay_token =
                crate::credentials::relay_key(origin)
                    .await
                    .map_err(|message| CommandError {
                        code: "keychain_error",
                        message,
                        retryable: true,
                    })?;
        }
    }
    let status = state
        .share_hub
        .start_named(request, key, name.unwrap_or_default())
        .await
        .map_err(CommandError::from)?;
    if let Err(error) = require_unlocked(&app).await {
        state.share_hub.stop().await;
        return Err(error);
    }
    if let (Some(origin), Some(token)) = (relay_origin, supplied_token) {
        if let Err(message) = crate::credentials::save_relay_key(&origin, token).await {
            state.share_hub.revoke(&status.id).await;
            return Err(CommandError {
                code: "keychain_error",
                message,
                retryable: true,
            });
        }
    }
    Ok(status)
}

/// Returns non-secret guest listener metadata.
#[tauri::command]
pub(crate) async fn share_status(state: State<'_, AppState>) -> Result<ManagedShare, CommandError> {
    Ok(state.share_hub.status().await)
}

#[tauri::command]
pub(crate) async fn list_shares(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<ManagedShare>, CommandError> {
    require_unlocked(&app).await?;
    Ok(state.share_hub.list().await)
}
#[tauri::command]
pub(crate) async fn revoke_share(
    id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), CommandError> {
    require_unlocked(&app).await?;
    state.share_hub.revoke(&id).await;
    Ok(())
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
    request
        .context_budget
        .check(&blackwall_core::model::messages(&request), &[])
        .map_err(|error| BridgeError::ModelError(error.to_string()))?;
    let messages = build_api_messages(&request)?;
    let payload = ApiCompletionRequest {
        model: &request.model,
        messages,
        stream: false,
        temperature: request.temperature,
        max_tokens: Some(
            request
                .max_tokens
                .unwrap_or(request.context_budget.output_tokens),
        ),
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
    api_key: Option<&str>,
    default_endpoint: &str,
    app: &AppHandle,
    request: ChatRequest,
    source: &[serde_json::Value],
    context: blackwall_core::context::ContextState,
) -> Result<(), CommandError> {
    let endpoint = selected_endpoint(&request, default_endpoint).map_err(CommandError::from)?;
    let backend = blackwall_core::model::HttpModel::new(
        &endpoint,
        &request.model,
        api_key.map(str::to_owned),
    )
    .and_then(|model| model.with_budget(request.context_budget))
    .map(|model| model.with_sampling(request.temperature, request.max_tokens))
    .map_err(CommandError::from)?;
    let instructions: Vec<_> = blackwall_core::model::messages(&request)
        .into_iter()
        .take(request.messages.len().saturating_sub(source.len()))
        .collect();
    blackwall_core::context::chat(
        &backend,
        &request.request_id,
        blackwall_core::context::ContextRun {
            source: source.to_vec(),
            state: context,
            budget: request.context_budget,
            instructions,
            compact: request.compact,
        },
        &|event| {
            let _ = emit_event(app, &event);
        },
    )
    .await
    .map(|_| ())
    .map_err(CommandError::from)
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
    if parsed.query().is_some() || parsed.fragment().is_some() || endpoint.len() > 2048 {
        return Err(BridgeError::InvalidEndpoint {
            endpoint: "model address".into(),
            reason: "Use an address without query parameters or fragments, at most 2,048 bytes."
                .into(),
        });
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn public_model_failures_classify_recovery_without_exposing_upstream_bodies() {
        for (status, code, retryable) in [
            (StatusCode::SERVICE_UNAVAILABLE, "model_unavailable", true),
            (StatusCode::FORBIDDEN, "model_access_denied", false),
            (StatusCode::GONE, "model_access_denied", false),
            (StatusCode::GATEWAY_TIMEOUT, "model_timeout", true),
            (StatusCode::TOO_MANY_REQUESTS, "model_busy", true),
            (StatusCode::NOT_FOUND, "model_not_found", false),
        ] {
            let error = CommandError::from_bridge(BridgeError::HttpStatus {
                status,
                body: "private upstream body https://private.example/?key=secret".into(),
            });
            assert_eq!(error.code, code);
            assert_eq!(error.retryable, retryable);
            assert!(!error.message.contains("private"));
            assert!(!error.message.contains("secret"));
        }
    }

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
            context_budget: Default::default(),
            context_state: None,
            compact: false,
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
