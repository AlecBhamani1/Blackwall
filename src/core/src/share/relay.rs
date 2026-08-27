//! Outbound hosted-relay connection owned by the desktop host.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch, Semaphore},
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
    MaybeTlsStream, WebSocketStream,
};

use super::relay_protocol::{
    HostToRelay, RelayRegistration, RelayToHost, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const OUTBOUND_QUEUE_CAPACITY: usize = 64;
const RESPONSE_CHUNK_BYTES: usize = 64 * 1024;

type RelaySocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Immutable inputs for one host-to-relay session.
#[derive(Clone)]
pub(crate) struct RelayHostConfig {
    pub(crate) socket_url: String,
    pub(crate) registration: RelayRegistration,
    pub(crate) client: reqwest::Client,
    pub(crate) upstream_endpoint: String,
    pub(crate) upstream_api_key: Option<String>,
}

/// Host-visible liveness and request counters.
#[derive(Clone)]
pub(crate) struct RelayHostControl {
    connected: Arc<AtomicBool>,
    request_count: Arc<AtomicU64>,
}

impl RelayHostControl {
    pub(crate) fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub(crate) fn request_count(&self) -> u64 {
        self.request_count.load(Ordering::Relaxed)
    }
}

/// Running outbound connection and its explicit shutdown path.
pub(crate) struct RunningRelayHost {
    pub(crate) control: RelayHostControl,
    pub(crate) shutdown: watch::Sender<bool>,
    pub(crate) task: JoinHandle<Result<(), RelayHostError>>,
}

/// Connects to the relay and waits until the share is published.
pub(crate) async fn start_relay_host(
    config: RelayHostConfig,
) -> Result<RunningRelayHost, RelayHostError> {
    let socket = connect_and_register(&config).await?;
    let control = RelayHostControl {
        connected: Arc::new(AtomicBool::new(true)),
        request_count: Arc::new(AtomicU64::new(0)),
    };
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task_control = control.clone();
    let task =
        tokio::spawn(
            async move { run_relay_host(config, socket, task_control, shutdown_rx).await },
        );
    Ok(RunningRelayHost {
        control,
        shutdown,
        task,
    })
}

async fn connect_and_register(config: &RelayHostConfig) -> Result<RelaySocket, RelayHostError> {
    let request = config
        .socket_url
        .clone()
        .into_client_request()
        .map_err(|error| RelayHostError::InvalidSocketUrl(error.to_string()))?;
    let (mut socket, _response) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request))
        .await
        .map_err(|_| RelayHostError::ConnectTimeout)?
        .map_err(RelayHostError::WebSocket)?;
    send_direct(
        &mut socket,
        &HostToRelay::Register {
            registration: config.registration.clone(),
        },
    )
    .await?;

    let message = tokio::time::timeout(CONNECT_TIMEOUT, socket.next())
        .await
        .map_err(|_| RelayHostError::RegistrationTimeout)?
        .ok_or(RelayHostError::Disconnected)?
        .map_err(RelayHostError::WebSocket)?;
    match parse_relay_message(message)? {
        Some(RelayToHost::Registered) => Ok(socket),
        Some(RelayToHost::Error { message }) => Err(RelayHostError::Rejected(message)),
        Some(_) | None => Err(RelayHostError::Protocol(
            "relay did not acknowledge the host registration".to_owned(),
        )),
    }
}

async fn run_relay_host(
    config: RelayHostConfig,
    mut socket: RelaySocket,
    control: RelayHostControl,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RelayHostError> {
    loop {
        control.connected.store(true, Ordering::Release);
        let connection_result = run_connection(&config, socket, &control, shutdown.clone()).await;
        control.connected.store(false, Ordering::Release);

        if *shutdown.borrow() || unix_epoch_millis() >= config.registration.expires_at_ms {
            return Ok(());
        }
        if matches!(connection_result, Err(RelayHostError::Rejected(_))) {
            return connection_result;
        }

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                () = tokio::time::sleep(RECONNECT_DELAY) => {}
            }
            if unix_epoch_millis() >= config.registration.expires_at_ms {
                return Ok(());
            }
            match connect_and_register(&config).await {
                Ok(reconnected) => {
                    socket = reconnected;
                    break;
                }
                Err(RelayHostError::Rejected(message)) => {
                    return Err(RelayHostError::Rejected(message));
                }
                Err(_) => continue,
            }
        }
    }
}

async fn run_connection(
    config: &RelayHostConfig,
    socket: RelaySocket,
    control: &RelayHostControl,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RelayHostError> {
    let (mut sink, mut source) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE_CAPACITY);
    let permits = Arc::new(Semaphore::new(2));
    let mut jobs = JoinSet::new();

    let result = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _close_result = sink.send(Message::Close(None)).await;
                    break Ok(());
                }
            }
            outbound = outbound_rx.recv() => {
                let Some(message) = outbound else {
                    break Err(RelayHostError::Disconnected);
                };
                if let Err(error) = sink.send(message).await {
                    break Err(RelayHostError::WebSocket(error));
                }
            }
            incoming = source.next() => {
                let Some(incoming) = incoming else {
                    break Err(RelayHostError::Disconnected);
                };
                let message = match incoming {
                    Ok(message) => message,
                    Err(error) => break Err(RelayHostError::WebSocket(error)),
                };
                if let Message::Ping(payload) = &message {
                    if outbound_tx.send(Message::Pong(payload.clone())).await.is_err() {
                        break Err(RelayHostError::Disconnected);
                    }
                    continue;
                }
                let Some(message) = parse_relay_message(message)? else {
                    continue;
                };
                match message {
                    RelayToHost::ChatRequest { request_id, body } => {
                        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                            send_error_response(
                                &outbound_tx,
                                request_id,
                                429,
                                "The host is already handling two guest requests.",
                            ).await?;
                            continue;
                        };
                        let job_config = config.clone();
                        let job_tx = outbound_tx.clone();
                        jobs.spawn(async move {
                            let _permit = permit;
                            if relay_chat(job_config, request_id.clone(), body, &job_tx).await.is_err() {
                                let _error_result = send_error_response(
                                    &job_tx,
                                    request_id,
                                    502,
                                    "The host could not complete the model request.",
                                ).await;
                            }
                        });
                    }
                    RelayToHost::RequestAccepted => {
                        control.request_count.fetch_add(1, Ordering::Relaxed);
                    }
                    RelayToHost::Error { message } => {
                        break Err(RelayHostError::Rejected(message));
                    }
                    RelayToHost::Registered => {}
                }
            }
            completed = jobs.join_next(), if !jobs.is_empty() => {
                if let Some(Err(error)) = completed {
                    if error.is_panic() {
                        break Err(RelayHostError::Protocol("a relayed model task panicked".to_owned()));
                    }
                }
            }
        }
    };

    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    result
}

async fn relay_chat(
    config: RelayHostConfig,
    request_id: String,
    encoded_body: String,
    outbound: &mpsc::Sender<Message>,
) -> Result<(), RelayHostError> {
    let body = URL_SAFE_NO_PAD
        .decode(encoded_body)
        .map_err(|_| RelayHostError::InvalidGuestRequest)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(RelayHostError::InvalidGuestRequest);
    }
    let mut payload: Value =
        serde_json::from_slice(&body).map_err(|_| RelayHostError::InvalidGuestRequest)?;
    let object = payload
        .as_object_mut()
        .ok_or(RelayHostError::InvalidGuestRequest)?;
    object.insert(
        "model".to_owned(),
        Value::String(config.registration.model.clone()),
    );

    let mut request = config
        .client
        .post(format!("{}/chat/completions", config.upstream_endpoint))
        .header(CONTENT_TYPE, "application/json")
        .json(&payload);
    if let Some(api_key) = config.upstream_api_key.as_deref() {
        request = request.bearer_auth(api_key);
    }
    let upstream = tokio::time::timeout(UPSTREAM_HEADER_TIMEOUT, request.send())
        .await
        .map_err(|_| RelayHostError::UpstreamTimeout)?
        .map_err(RelayHostError::Upstream)?;
    let status = upstream.status();
    if !status.is_success() {
        send_error_response(
            outbound,
            request_id,
            status.as_u16(),
            &format!("The host model rejected the request (HTTP {status})."),
        )
        .await?;
        return Ok(());
    }
    if upstream
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(RelayHostError::ResponseTooLarge);
    }

    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_owned();
    send_protocol(
        outbound,
        &HostToRelay::ResponseStart {
            request_id: request_id.clone(),
            status: status.as_u16(),
            content_type,
        },
    )
    .await?;

    let mut received = 0_usize;
    let mut stream = upstream.bytes_stream();
    loop {
        let next = match tokio::time::timeout(RESPONSE_IDLE_TIMEOUT, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                send_protocol(
                    outbound,
                    &HostToRelay::ResponseAbort {
                        request_id,
                        message: "The host model response became idle.".to_owned(),
                    },
                )
                .await?;
                return Ok(());
            }
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                send_protocol(
                    outbound,
                    &HostToRelay::ResponseAbort {
                        request_id,
                        message: "The host model stream was interrupted.".to_owned(),
                    },
                )
                .await?;
                return Ok(());
            }
        };
        received = received.saturating_add(chunk.len());
        if received > MAX_RESPONSE_BYTES {
            send_protocol(
                outbound,
                &HostToRelay::ResponseAbort {
                    request_id,
                    message: "The host model response exceeded the safety limit.".to_owned(),
                },
            )
            .await?;
            return Ok(());
        }
        for piece in chunk.chunks(RESPONSE_CHUNK_BYTES) {
            send_protocol(
                outbound,
                &HostToRelay::ResponseChunk {
                    request_id: request_id.clone(),
                    data: URL_SAFE_NO_PAD.encode(piece),
                },
            )
            .await?;
        }
    }
    send_protocol(outbound, &HostToRelay::ResponseEnd { request_id }).await
}

async fn send_error_response(
    outbound: &mpsc::Sender<Message>,
    request_id: String,
    status: u16,
    message: &str,
) -> Result<(), RelayHostError> {
    let status = if (400..=599).contains(&status) {
        status
    } else {
        502
    };
    send_protocol(
        outbound,
        &HostToRelay::ResponseStart {
            request_id: request_id.clone(),
            status,
            content_type: "application/json".to_owned(),
        },
    )
    .await?;
    let body = serde_json::to_vec(&json!({
        "error": {
            "message": message,
            "type": "blackwall_share_error"
        }
    }))
    .map_err(RelayHostError::Serialize)?;
    send_protocol(
        outbound,
        &HostToRelay::ResponseChunk {
            request_id: request_id.clone(),
            data: URL_SAFE_NO_PAD.encode(body),
        },
    )
    .await?;
    send_protocol(outbound, &HostToRelay::ResponseEnd { request_id }).await
}

async fn send_direct(
    socket: &mut RelaySocket,
    message: &HostToRelay,
) -> Result<(), RelayHostError> {
    let payload = serde_json::to_string(message).map_err(RelayHostError::Serialize)?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .map_err(RelayHostError::WebSocket)
}

async fn send_protocol(
    outbound: &mpsc::Sender<Message>,
    message: &HostToRelay,
) -> Result<(), RelayHostError> {
    let payload = serde_json::to_string(message).map_err(RelayHostError::Serialize)?;
    outbound
        .send(Message::Text(payload.into()))
        .await
        .map_err(|_| RelayHostError::Disconnected)
}

fn parse_relay_message(message: Message) -> Result<Option<RelayToHost>, RelayHostError> {
    match message {
        Message::Text(payload) => serde_json::from_str(payload.as_ref())
            .map(Some)
            .map_err(RelayHostError::Serialize),
        Message::Close(_) => Err(RelayHostError::Disconnected),
        Message::Binary(_) => Err(RelayHostError::Protocol(
            "relay sent an unsupported binary message".to_owned(),
        )),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
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

/// Failure while publishing or maintaining a hosted share.
#[derive(Debug, Error)]
pub(crate) enum RelayHostError {
    #[error("invalid relay WebSocket URL: {0}")]
    InvalidSocketUrl(String),
    #[error("timed out while connecting to the hosted relay")]
    ConnectTimeout,
    #[error("timed out while waiting for relay registration")]
    RegistrationTimeout,
    #[error("the hosted relay disconnected")]
    Disconnected,
    #[error("the hosted relay rejected this share: {0}")]
    Rejected(String),
    #[error("hosted relay protocol error: {0}")]
    Protocol(String),
    #[error("hosted relay WebSocket error: {0}")]
    WebSocket(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("hosted relay message encoding failed: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("the relay delivered an invalid guest request")]
    InvalidGuestRequest,
    #[error("the host model did not begin responding in time")]
    UpstreamTimeout,
    #[error("the host model is unavailable: {0}")]
    Upstream(#[source] reqwest::Error),
    #[error("the host model response exceeded the safety limit")]
    ResponseTooLarge,
}
