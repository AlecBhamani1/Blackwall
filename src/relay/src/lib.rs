//! Self-hostable public edge for Blackwall's outbound guest-sharing relay.

use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{
        rejection::{BytesRejection, PathRejection},
        ws::{Message, WebSocket, WebSocketUpgrade},
        FromRequestParts, Path, State,
    },
    http::{
        header::{
            AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY,
            X_CONTENT_TYPE_OPTIONS,
        },
        request::Parts,
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use blackwall_core::share::{
    guest_assets,
    relay_protocol::{
        HostToRelay, RelayRegistration, RelayToHost, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    },
    salted_key_hash,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, RwLock, Semaphore};

const PROTOCOL_VERSION: u16 = 1;
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_START_TIMEOUT: Duration = Duration::from_secs(35);
const RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_INVITE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const HOST_QUEUE_CAPACITY: usize = 16;
const BODY_QUEUE_CAPACITY: usize = 16;
const DEFAULT_MAX_SESSIONS: usize = 1_024;
const MAX_HOST_MESSAGE_BYTES: usize = 256 * 1024;
const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'none'; connect-src 'self'; img-src 'self' data: blob:; script-src 'self'; style-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'";

/// Runtime policy for one relay process.
#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// Optional shared secret required when a desktop registers a session.
    pub registration_token: Option<String>,
    /// Hard cap on simultaneously connected desktop sessions.
    pub max_sessions: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            registration_token: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }
}

#[derive(Clone)]
struct RelayState {
    sessions: Arc<RwLock<HashMap<String, Arc<RelaySession>>>>,
    config: RelayConfig,
}

struct RelaySession {
    host_key: String,
    model: String,
    salt: [u8; 32],
    key_hash: [u8; 32],
    expires_at_ms: u64,
    connected: AtomicBool,
    host_tx: mpsc::Sender<Message>,
    pending: StdMutex<HashMap<String, Arc<PendingResponse>>>,
    in_flight: Arc<Semaphore>,
}

struct PendingResponse {
    start: StdMutex<Option<oneshot::Sender<ResponseMetadata>>>,
    body: mpsc::Sender<Result<Bytes, io::Error>>,
    received_bytes: AtomicUsize,
    started: AtomicBool,
}

struct ResponseMetadata {
    status: StatusCode,
    content_type: HeaderValue,
}

/// Builds the relay HTTP/WebSocket application.
pub fn app(config: RelayConfig) -> Router {
    let state = RelayState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        config,
    };
    Router::new()
        .route("/health", get(health))
        .route("/v1/host/connect", get(host_connect))
        .route("/s/{session_id}/guest", get(guest_page))
        .route("/s/{session_id}/guest/style.css", get(guest_css))
        .route("/s/{session_id}/guest/app.js", get(guest_javascript))
        .route("/s/{session_id}/v1/models", get(models))
        .route(
            "/s/{session_id}/v1/chat/completions",
            post(chat_completions),
        )
        .fallback(not_found)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    no_store(Json(json!({ "status": "ok" })).into_response())
}

async fn host_connect(websocket: WebSocketUpgrade, State(state): State<RelayState>) -> Response {
    websocket
        .max_message_size(MAX_HOST_MESSAGE_BYTES)
        .max_frame_size(MAX_HOST_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_host(socket, state))
}

async fn handle_host(mut socket: WebSocket, state: RelayState) {
    let registration = match receive_registration(&mut socket).await {
        Ok(registration) => registration,
        Err(message) => {
            let _send_result =
                send_socket_protocol(&mut socket, &RelayToHost::Error { message }).await;
            let _close_result = socket.close().await;
            return;
        }
    };
    if let Err(message) = validate_registration(&registration, &state.config) {
        let _send_result = send_socket_protocol(&mut socket, &RelayToHost::Error { message }).await;
        let _close_result = socket.close().await;
        return;
    }
    let Some(salt) = decode_32(&registration.guest_key_salt) else {
        let _send_result = send_socket_protocol(
            &mut socket,
            &RelayToHost::Error {
                message: "guest key salt is invalid".to_owned(),
            },
        )
        .await;
        return;
    };
    let Some(key_hash) = decode_32(&registration.guest_key_hash) else {
        let _send_result = send_socket_protocol(
            &mut socket,
            &RelayToHost::Error {
                message: "guest key digest is invalid".to_owned(),
            },
        )
        .await;
        return;
    };

    let (host_tx, mut host_rx) = mpsc::channel(HOST_QUEUE_CAPACITY);
    let session = Arc::new(RelaySession {
        host_key: registration.host_key.clone(),
        model: registration.model,
        salt,
        key_hash,
        expires_at_ms: registration.expires_at_ms,
        connected: AtomicBool::new(true),
        host_tx,
        pending: StdMutex::new(HashMap::new()),
        in_flight: Arc::new(Semaphore::new(2)),
    });
    let replaced = {
        let mut sessions = state.sessions.write().await;
        if let Some(existing) = sessions.get(&registration.session_id) {
            if !constant_time_text_eq(&existing.host_key, &registration.host_key) {
                drop(sessions);
                let _send_result = send_socket_protocol(
                    &mut socket,
                    &RelayToHost::Error {
                        message: "session identifier is already registered".to_owned(),
                    },
                )
                .await;
                return;
            }
        } else if sessions.len() >= state.config.max_sessions {
            drop(sessions);
            let _send_result = send_socket_protocol(
                &mut socket,
                &RelayToHost::Error {
                    message: "relay session capacity has been reached".to_owned(),
                },
            )
            .await;
            return;
        }
        sessions.insert(registration.session_id.clone(), Arc::clone(&session))
    };
    if let Some(previous) = replaced {
        previous
            .disconnect("The host reconnected through a new relay connection.")
            .await;
    }
    if send_socket_protocol(&mut socket, &RelayToHost::Registered)
        .await
        .is_err()
    {
        remove_session(&state, &registration.session_id, &session).await;
        return;
    }

    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            outgoing = host_rx.recv() => {
                let Some(outgoing) = outgoing else { break };
                if sender.send(outgoing).await.is_err() {
                    break;
                }
            }
            incoming = receiver.next() => {
                let Some(incoming) = incoming else { break };
                let message = match incoming {
                    Ok(message) => message,
                    Err(_) => break,
                };
                match message {
                    Message::Text(payload) => {
                        let parsed = serde_json::from_str::<HostToRelay>(payload.as_ref());
                        let Ok(parsed) = parsed else {
                            let _error_result = session.send_to_host(&RelayToHost::Error {
                                message: "host sent an invalid relay message".to_owned(),
                            }).await;
                            break;
                        };
                        if matches!(parsed, HostToRelay::Register { .. }) {
                            let _error_result = session.send_to_host(&RelayToHost::Error {
                                message: "host attempted to register twice".to_owned(),
                            }).await;
                            break;
                        }
                        handle_host_message(&session, parsed).await;
                    }
                    Message::Ping(payload) => {
                        if session.host_tx.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Binary(_) | Message::Pong(_) => {}
                }
            }
        }
    }

    session.disconnect("The Blackwall host disconnected.").await;
    remove_session(&state, &registration.session_id, &session).await;
}

async fn receive_registration(socket: &mut WebSocket) -> Result<RelayRegistration, String> {
    let message = tokio::time::timeout(REGISTRATION_TIMEOUT, socket.recv())
        .await
        .map_err(|_| "host registration timed out".to_owned())?
        .ok_or_else(|| "host disconnected before registration".to_owned())?
        .map_err(|_| "host registration could not be read".to_owned())?;
    let Message::Text(payload) = message else {
        return Err("the first host message must be a registration".to_owned());
    };
    match serde_json::from_str::<HostToRelay>(payload.as_ref())
        .map_err(|_| "host registration is invalid".to_owned())?
    {
        HostToRelay::Register { registration } => Ok(registration),
        _ => Err("the first host message must be a registration".to_owned()),
    }
}

fn validate_registration(
    registration: &RelayRegistration,
    config: &RelayConfig,
) -> Result<(), String> {
    if registration.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "unsupported relay protocol version {}",
            registration.protocol_version
        ));
    }
    if !valid_random_identifier(&registration.session_id, "bws_")
        || !valid_random_identifier(&registration.host_key, "bwh_")
    {
        return Err("session credentials are malformed".to_owned());
    }
    let model = registration.model.trim();
    if model.is_empty() || model.len() > 512 {
        return Err("the pinned model identifier is invalid".to_owned());
    }
    let now = unix_epoch_millis();
    let maximum_expiry = now.saturating_add(
        MAX_INVITE_LIFETIME
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    );
    if registration.expires_at_ms <= now || registration.expires_at_ms > maximum_expiry {
        return Err("the invite expiry is outside the allowed window".to_owned());
    }
    if let Some(expected) = config.registration_token.as_deref() {
        let presented = registration.relay_token.as_deref().unwrap_or("");
        if !constant_time_text_eq(expected, presented) {
            return Err("the relay registration token is invalid".to_owned());
        }
    }
    Ok(())
}

fn valid_random_identifier(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|encoded| URL_SAFE_NO_PAD.decode(encoded).ok())
        .is_some_and(|bytes| bytes.len() == 32)
}

fn decode_32(value: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()
}

async fn handle_host_message(session: &Arc<RelaySession>, message: HostToRelay) {
    match message {
        HostToRelay::ResponseStart {
            request_id,
            status,
            content_type,
        } => {
            if let Some(pending) = session.pending(&request_id) {
                pending.start(status, &content_type);
            }
        }
        HostToRelay::ResponseChunk { request_id, data } => {
            let Some(pending) = session.pending(&request_id) else {
                return;
            };
            let Ok(chunk) = URL_SAFE_NO_PAD.decode(data) else {
                session
                    .abort_pending(&request_id, "The host sent an invalid response chunk.")
                    .await;
                return;
            };
            if pending.push(Bytes::from(chunk)).await.is_err() {
                session.remove_pending(&request_id);
            }
        }
        HostToRelay::ResponseEnd { request_id } => {
            session.remove_pending(&request_id);
        }
        HostToRelay::ResponseAbort {
            request_id,
            message,
        } => {
            session.abort_pending(&request_id, &message).await;
        }
        HostToRelay::Register { .. } => {}
    }
}

impl RelaySession {
    fn is_available(&self) -> bool {
        self.connected.load(Ordering::Acquire) && unix_epoch_millis() < self.expires_at_ms
    }

    async fn send_to_host(&self, message: &RelayToHost) -> Result<(), ()> {
        let payload = serde_json::to_string(message).map_err(|_| ())?;
        self.host_tx
            .send(Message::Text(payload.into()))
            .await
            .map_err(|_| ())
    }

    fn pending(&self, request_id: &str) -> Option<Arc<PendingResponse>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(request_id)
            .cloned()
    }

    fn insert_pending(&self, request_id: String, pending: Arc<PendingResponse>) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request_id, pending);
    }

    fn remove_pending(&self, request_id: &str) -> Option<Arc<PendingResponse>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(request_id)
    }

    async fn abort_pending(&self, request_id: &str, message: &str) {
        if let Some(pending) = self.remove_pending(request_id) {
            pending.abort(message).await;
        }
    }

    async fn disconnect(&self, message: &str) {
        self.connected.store(false, Ordering::Release);
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending
                .drain()
                .map(|(_, pending)| pending)
                .collect::<Vec<_>>()
        };
        for response in pending {
            response.abort(message).await;
        }
    }
}

impl PendingResponse {
    fn start(&self, status: u16, content_type: &str) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        let content_type = HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
        if let Some(start) = self
            .start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _send_result = start.send(ResponseMetadata {
                status,
                content_type,
            });
        }
    }

    async fn push(&self, chunk: Bytes) -> Result<(), ()> {
        if !self.started.load(Ordering::Acquire) {
            return Err(());
        }
        let received = self
            .received_bytes
            .fetch_add(chunk.len(), Ordering::AcqRel)
            .saturating_add(chunk.len());
        if received > MAX_RESPONSE_BYTES {
            self.abort("The relayed model response exceeded the safety limit.")
                .await;
            return Err(());
        }
        self.body.send(Ok(chunk)).await.map_err(|_| ())
    }

    async fn abort(&self, message: &str) {
        if !self.started.load(Ordering::Acquire) {
            self.start(StatusCode::BAD_GATEWAY.as_u16(), "application/json");
        }
        let _send_result = self
            .body
            .send(Err(io::Error::other(message.to_owned())))
            .await;
    }
}

async fn remove_session(state: &RelayState, session_id: &str, session: &Arc<RelaySession>) {
    let mut sessions = state.sessions.write().await;
    if sessions
        .get(session_id)
        .is_some_and(|registered| Arc::ptr_eq(registered, session))
    {
        sessions.remove(session_id);
    }
}

async fn guest_page(Path(session_id): Path<String>, State(state): State<RelayState>) -> Response {
    static_for_session(
        &state,
        &session_id,
        Html(guest_assets::HTML),
        "text/html; charset=utf-8",
    )
    .await
}

async fn guest_css(Path(session_id): Path<String>, State(state): State<RelayState>) -> Response {
    static_for_session(
        &state,
        &session_id,
        guest_assets::CSS,
        "text/css; charset=utf-8",
    )
    .await
}

async fn guest_javascript(
    Path(session_id): Path<String>,
    State(state): State<RelayState>,
) -> Response {
    static_for_session(
        &state,
        &session_id,
        guest_assets::JAVASCRIPT,
        "text/javascript; charset=utf-8",
    )
    .await
}

async fn static_for_session(
    state: &RelayState,
    session_id: &str,
    body: impl IntoResponse,
    content_type: &'static str,
) -> Response {
    let available = state
        .sessions
        .read()
        .await
        .get(session_id)
        .is_some_and(|session| session.is_available());
    if !available {
        return RelayApiError::not_found("This Blackwall share is unavailable.").into_response();
    }
    static_response(body, content_type)
}

fn static_response(body: impl IntoResponse, content_type: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

struct GuestAccess(Arc<RelaySession>);

impl FromRequestParts<RelayState> for GuestAccess {
    type Rejection = RelayApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RelayState,
    ) -> Result<Self, Self::Rejection> {
        let Path(session_id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(path_rejection)?;
        let session = state
            .sessions
            .read()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| RelayApiError::not_found("This Blackwall share is unavailable."))?;
        authorize(&session, &parts.headers)?;
        Ok(Self(session))
    }
}

struct ChatAccess {
    session: Arc<RelaySession>,
    permit: OwnedSemaphorePermit,
}

impl FromRequestParts<RelayState> for ChatAccess {
    type Rejection = RelayApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RelayState,
    ) -> Result<Self, Self::Rejection> {
        let GuestAccess(session) = GuestAccess::from_request_parts(parts, state).await?;
        let permit = Arc::clone(&session.in_flight)
            .try_acquire_owned()
            .map_err(|_| {
                RelayApiError::too_many_requests(
                    "The host is handling two guest requests. Try again shortly.",
                )
            })?;
        Ok(Self { session, permit })
    }
}

fn path_rejection(_rejection: PathRejection) -> RelayApiError {
    RelayApiError::not_found("This Blackwall share is unavailable.")
}

fn authorize(session: &RelaySession, headers: &HeaderMap) -> Result<(), RelayApiError> {
    if unix_epoch_millis() >= session.expires_at_ms {
        return Err(RelayApiError::unauthorized("This invite has expired."));
    }
    if !session.connected.load(Ordering::Acquire) {
        return Err(RelayApiError::unavailable(
            "The Blackwall host is reconnecting.",
        ));
    }
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let presented_hash = salted_key_hash(&session.salt, presented);
    if presented_hash.ct_eq(&session.key_hash).unwrap_u8() != 1 {
        return Err(RelayApiError::unauthorized("The invite key is invalid."));
    }
    Ok(())
}

async fn models(GuestAccess(session): GuestAccess) -> Result<Response, RelayApiError> {
    session
        .send_to_host(&RelayToHost::RequestAccepted)
        .await
        .map_err(|()| RelayApiError::unavailable("The Blackwall host disconnected."))?;
    Ok(no_store(
        Json(json!({
            "object": "list",
            "data": [{
                "id": session.model,
                "object": "model",
                "created": 0,
                "owned_by": "blackwall-host"
            }]
        }))
        .into_response(),
    ))
}

async fn chat_completions(
    ChatAccess { session, permit }: ChatAccess,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, RelayApiError> {
    let body = body.map_err(|rejection| {
        let message = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            "The chat request exceeded the 32 MB safety limit."
        } else {
            "The chat request body could not be read."
        };
        RelayApiError::new(rejection.status(), message)
    })?;
    let mut payload: Value = serde_json::from_slice(&body)
        .map_err(|_| RelayApiError::bad_request("The chat request must be valid JSON."))?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| RelayApiError::bad_request("The chat request must be a JSON object."))?;
    object.insert("model".to_owned(), Value::String(session.model.clone()));
    let body = serde_json::to_vec(&payload)
        .map_err(|_| RelayApiError::bad_request("The chat request could not be encoded."))?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(RelayApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "The chat request exceeded the 32 MB safety limit.",
        ));
    }

    let request_id = random_request_id();
    let (start_tx, start_rx) = oneshot::channel();
    let (body_tx, mut body_rx) = mpsc::channel(BODY_QUEUE_CAPACITY);
    session.insert_pending(
        request_id.clone(),
        Arc::new(PendingResponse {
            start: StdMutex::new(Some(start_tx)),
            body: body_tx,
            received_bytes: AtomicUsize::new(0),
            started: AtomicBool::new(false),
        }),
    );
    let delivery = session
        .send_to_host(&RelayToHost::ChatRequest {
            request_id: request_id.clone(),
            body: URL_SAFE_NO_PAD.encode(body),
        })
        .await;
    if delivery.is_err() {
        session.remove_pending(&request_id);
        return Err(RelayApiError::unavailable(
            "The Blackwall host disconnected.",
        ));
    }
    let _count_result = session.send_to_host(&RelayToHost::RequestAccepted).await;

    let metadata = match tokio::time::timeout(RESPONSE_START_TIMEOUT, start_rx).await {
        Ok(Ok(metadata)) => metadata,
        Ok(Err(_)) => {
            session.remove_pending(&request_id);
            return Err(RelayApiError::bad_gateway(
                "The Blackwall host closed the request before responding.",
            ));
        }
        Err(_) => {
            session.remove_pending(&request_id);
            return Err(RelayApiError::bad_gateway(
                "The Blackwall host did not begin responding in time.",
            ));
        }
    };

    let response_session = Arc::clone(&session);
    let response_request_id = request_id.clone();
    let stream = async_stream::stream! {
        let _permit = permit;
        loop {
            let next = match tokio::time::timeout(RESPONSE_IDLE_TIMEOUT, body_rx.recv()).await {
                Ok(next) => next,
                Err(_) => {
                    yield Err::<Bytes, io::Error>(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "relayed response became idle",
                    ));
                    break;
                }
            };
            let Some(chunk) = next else { break };
            yield chunk;
        }
        response_session.remove_pending(&response_request_id);
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = metadata.status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, metadata.content_type);
    Ok(no_store(response))
}

fn random_request_id() -> String {
    use rand::{rngs::OsRng, RngCore};

    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn send_socket_protocol(socket: &mut WebSocket, message: &RelayToHost) -> Result<(), ()> {
    let payload = serde_json::to_string(message).map_err(|_| ())?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .map_err(|_| ())
}

fn constant_time_text_eq(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    expected.as_bytes().ct_eq(presented.as_bytes()).unwrap_u8() == 1
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

async fn not_found() -> RelayApiError {
    RelayApiError::not_found("Route not found.")
}

#[derive(Debug)]
struct RelayApiError {
    status: StatusCode,
    message: String,
}

impl RelayApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }
}

impl IntoResponse for RelayApiError {
    fn into_response(self) -> Response {
        no_store(
            (
                self.status,
                Json(json!({
                    "error": {
                        "message": self.message,
                        "type": "blackwall_share_error"
                    }
                })),
            )
                .into_response(),
        )
    }
}

fn unix_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use blackwall_core::share::relay_protocol::{HostToRelay, RelayRegistration, RelayToHost};
    use blackwall_core::share::{ShareHub, StartShareRequest};
    use futures_util::{SinkExt, StreamExt};
    use reqwest::header::AUTHORIZATION;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

    async fn next_protocol(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> RelayToHost {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            if let ClientMessage::Text(payload) = message {
                return serde_json::from_str(payload.as_ref()).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn relays_an_authenticated_stream_from_host_to_guest() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app(RelayConfig::default()))
                .await
                .unwrap();
        });

        let guest_key = "bw1_test-guest-key";
        let salt = [7_u8; 32];
        let key_hash = salted_key_hash(&salt, guest_key);
        let registration = RelayRegistration {
            protocol_version: 1,
            session_id: format!("bws_{}", URL_SAFE_NO_PAD.encode([1_u8; 32])),
            host_key: format!("bwh_{}", URL_SAFE_NO_PAD.encode([2_u8; 32])),
            relay_token: None,
            model: "test-model".to_owned(),
            guest_key_salt: URL_SAFE_NO_PAD.encode(salt),
            guest_key_hash: URL_SAFE_NO_PAD.encode(key_hash),
            expires_at_ms: unix_epoch_millis() + 60_000,
        };
        let (mut host, _) = connect_async(format!("ws://{address}/v1/host/connect"))
            .await
            .unwrap();
        host.send(ClientMessage::Text(
            serde_json::to_string(&HostToRelay::Register {
                registration: registration.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        assert_eq!(next_protocol(&mut host).await, RelayToHost::Registered);

        let client = reqwest::Client::new();
        let models = client
            .get(format!(
                "http://{address}/s/{}/v1/models",
                registration.session_id
            ))
            .header(AUTHORIZATION, format!("Bearer {guest_key}"))
            .send()
            .await
            .unwrap();
        assert_eq!(models.status(), StatusCode::OK);

        let chat_client = client.clone();
        let chat_url = format!(
            "http://{address}/s/{}/v1/chat/completions",
            registration.session_id
        );
        let guest_chat = tokio::spawn(async move {
            chat_client
                .post(chat_url)
                .header(AUTHORIZATION, format!("Bearer {guest_key}"))
                .json(&json!({
                    "model": "guest-choice",
                    "messages": [{"role": "user", "content": "hello"}],
                    "stream": true
                }))
                .send()
                .await
                .unwrap()
        });

        let request_id = loop {
            if let RelayToHost::ChatRequest { request_id, body } = next_protocol(&mut host).await {
                let decoded = URL_SAFE_NO_PAD.decode(body).unwrap();
                let payload: Value = serde_json::from_slice(&decoded).unwrap();
                assert_eq!(payload["model"], "test-model");
                break request_id;
            }
        };
        let response_body =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
        for message in [
            HostToRelay::ResponseStart {
                request_id: request_id.clone(),
                status: 200,
                content_type: "text/event-stream".to_owned(),
            },
            HostToRelay::ResponseChunk {
                request_id: request_id.clone(),
                data: URL_SAFE_NO_PAD.encode(response_body),
            },
            HostToRelay::ResponseEnd { request_id },
        ] {
            host.send(ClientMessage::Text(
                serde_json::to_string(&message).unwrap().into(),
            ))
            .await
            .unwrap();
        }

        let response = guest_chat.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), response_body);
        server.abort();
    }

    async fn canned_model(Json(mut payload): Json<Value>) -> Response {
        let model = payload
            .as_object_mut()
            .and_then(|object| object.remove("model"))
            .and_then(|model| model.as_str().map(str::to_owned))
            .unwrap_or_default();
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"from {model}\"}}}}]}}\n\ndata: [DONE]\n\n"
            )))
            .unwrap()
    }

    #[tokio::test]
    async fn desktop_share_hub_streams_end_to_end_through_a_token_protected_relay() {
        let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_address = model_listener.local_addr().unwrap();
        let model_server = tokio::spawn(async move {
            axum::serve(
                model_listener,
                Router::new().route("/v1/chat/completions", post(canned_model)),
            )
            .await
            .unwrap();
        });
        let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = relay_listener.local_addr().unwrap();
        let relay_server = tokio::spawn(async move {
            axum::serve(
                relay_listener,
                app(RelayConfig {
                    registration_token: Some("deployment-token".to_owned()),
                    ..RelayConfig::default()
                }),
            )
            .await
            .unwrap();
        });

        let hub = ShareHub::new();
        let status = hub
            .start(StartShareRequest {
                model: "owner-pinned-model".to_owned(),
                endpoint: Some(format!("http://{model_address}/v1")),
                relay_url: Some(format!("http://{relay_address}")),
                relay_token: Some("deployment-token".to_owned()),
                expires_in_minutes: 1,
            })
            .await
            .unwrap();
        let mut invite = reqwest::Url::parse(&status.share_url).unwrap();
        let guest_key = invite
            .fragment()
            .unwrap()
            .strip_prefix("key=")
            .unwrap()
            .to_owned();
        invite.set_fragment(None);
        invite.set_path(&invite.path().replace("/guest", "/v1/chat/completions"));
        let response = reqwest::Client::new()
            .post(invite)
            .bearer_auth(guest_key)
            .json(&json!({
                "model": "guest-tried-to-change-model",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("from owner-pinned-model"));
        assert!(!body.contains("guest-tried-to-change-model"));

        hub.stop().await;
        relay_server.abort();
        model_server.abort();
    }
}
