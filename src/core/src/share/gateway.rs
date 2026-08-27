//! Authenticated, bounded OpenAI-compatible guest gateway.

use std::{
    io::{self, Read, Write},
    net::{
        Ipv4Addr, Shutdown, SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream,
    },
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{rejection::BytesRejection, DefaultBodyLimit, FromRequestParts, State},
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
use futures_util::StreamExt;
use nix::{
    errno::Errno,
    poll::{poll, PollFd, PollFlags, PollTimeout},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener as TokioTcpListener,
    sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};

const GUEST_HTML: &str = include_str!("assets/guest.html");
const GUEST_CSS: &str = include_str!("assets/guest.css");
const GUEST_JS: &str = include_str!("assets/guest.js");
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_UPSTREAM_ERROR_BYTES: usize = 16 * 1024;
const LISTENER_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const STARTUP_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const STARTUP_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_PROXY_CONNECTIONS: usize = 32;
const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PROXY_IO_TIMEOUT: Duration = Duration::from_secs(120);
const PROXY_POLL_TIMEOUT_MILLIS: u16 = 1_000;
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const UPSTREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'none'; connect-src 'self'; img-src 'self' data: blob:; script-src 'self'; style-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'";

/// Immutable inputs needed to create one gateway listener.
pub(crate) struct GatewayConfig {
    pub(crate) client: reqwest::Client,
    pub(crate) upstream_endpoint: String,
    pub(crate) upstream_api_key: Option<String>,
    pub(crate) pinned_model: String,
    pub(crate) bind_ip: Ipv4Addr,
    pub(crate) port: u16,
    pub(crate) salt: [u8; 32],
    pub(crate) key_hash: [u8; 32],
    pub(crate) expires_at_ms: u64,
}

/// Runtime controls and counters that do not retain the raw invite key.
#[derive(Clone)]
pub(crate) struct GatewayControl {
    state: Arc<GatewayState>,
}

impl GatewayControl {
    pub(crate) fn revoke(&self) {
        self.state.revoked.store(true, Ordering::Release);
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.state.revoked.load(Ordering::Acquire)
            && unix_epoch_millis() < self.state.expires_at_ms
    }

    pub(crate) fn request_count(&self) -> u64 {
        self.state.request_count.load(Ordering::Relaxed)
    }
}

/// A running server plus its explicit shutdown path.
pub(crate) struct RunningGateway {
    pub(crate) local_addr: SocketAddr,
    pub(crate) control: GatewayControl,
    pub(crate) shutdown: oneshot::Sender<()>,
    pub(crate) task: JoinHandle<io::Result<()>>,
}

struct GatewayState {
    client: reqwest::Client,
    upstream_endpoint: String,
    upstream_api_key: Option<String>,
    pinned_model: String,
    salt: [u8; 32],
    key_hash: [u8; 32],
    expires_at_ms: u64,
    revoked: AtomicBool,
    request_count: AtomicU64,
    in_flight: Arc<Semaphore>,
}

/// Starts the listener. The caller must keep and eventually use the returned
/// shutdown sender; dropping a Tokio join handle alone does not stop a task.
pub(crate) async fn start_gateway(config: GatewayConfig) -> io::Result<RunningGateway> {
    let state = Arc::new(GatewayState {
        client: config.client,
        upstream_endpoint: config.upstream_endpoint,
        upstream_api_key: config.upstream_api_key,
        pinned_model: config.pinned_model,
        salt: config.salt,
        key_hash: config.key_hash,
        expires_at_ms: config.expires_at_ms,
        revoked: AtomicBool::new(false),
        request_count: AtomicU64::new(0),
        in_flight: Arc::new(Semaphore::new(2)),
    });
    let router = router(Arc::clone(&state));
    let public_listener = StdTcpListener::bind(SocketAddr::from((config.bind_ip, config.port)))?;
    let local_addr = public_listener.local_addr()?;
    let internal_listener = TokioTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let internal_addr = internal_listener.local_addr()?;
    let (shutdown, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run_gateway(
        public_listener,
        internal_listener,
        internal_addr,
        router,
        shutdown_rx,
    ));

    // Do not advertise an invite until the complete public-to-loopback path
    // has served a real request. The retry window also lets a sleeping
    // Tailscale network extension re-establish its local route.
    if let Err(error) = verify_listener_ready(local_addr).await {
        let _shutdown_result = shutdown.send(());
        // The supervisor owns a running blocking bridge that Tokio cannot
        // cancel safely. Let it wake, close, and join its resources instead of
        // detaching it behind a competing outer timeout.
        let _gateway_result = task.await;
        return Err(error);
    }

    Ok(RunningGateway {
        local_addr,
        control: GatewayControl { state },
        shutdown,
        task,
    })
}

/// Runs Axum on an internal loopback socket and bridges the exact externally
/// bound address with standard blocking streams. On affected macOS/Tailscale
/// combinations, kqueue never reports listener or accepted-stream readiness
/// for a `utun` IPv4. Keeping those sockets outside mio avoids that OS edge
/// case without broadening the public bind address.
async fn run_gateway(
    public_listener: StdTcpListener,
    internal_listener: TokioTcpListener,
    internal_addr: SocketAddr,
    router: Router,
    shutdown: oneshot::Receiver<()>,
) -> io::Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let server_stop = stop_rx.clone();
    // Complete every fallible bridge setup step before spawning Axum so an
    // early return cannot detach the internal server.
    let (wake_reader, wake_writer) = UnixStream::pair()?;
    wake_reader.set_nonblocking(true)?;
    wake_writer.set_nonblocking(true)?;
    let mut server = tokio::spawn(async move {
        axum::serve(internal_listener, router)
            .with_graceful_shutdown(wait_for_stop(server_stop))
            .await
    });
    let proxy_control = Arc::new(ProxyControl {
        stopped: AtomicBool::new(false),
        active_connections: Arc::new(ActiveConnections::default()),
        wake_writer: StdMutex::new(wake_writer),
    });
    let mut proxy = tokio::task::spawn_blocking({
        let proxy_control = Arc::clone(&proxy_control);
        move || {
            proxy_accept_loop_blocking(public_listener, internal_addr, wake_reader, proxy_control)
        }
    });
    tokio::pin!(shutdown);

    tokio::select! {
        result = &mut server => {
            let server_result = flatten_task_result(result);
            signal_gateway_stop(&stop_tx, &proxy_control);
            let proxy_result = flatten_task_result(proxy.await);
            server_result.and(proxy_result)
        }
        result = &mut proxy => {
            let proxy_result = flatten_task_result(result);
            signal_gateway_stop(&stop_tx, &proxy_control);
            let server_result = await_server_with_timeout(&mut server).await;
            proxy_result.and(server_result)
        }
        _ = &mut shutdown => {
            signal_gateway_stop(&stop_tx, &proxy_control);
            // The blocking bridge owns the public listener. Its wake socket,
            // atomic registration gate, and tracked socket pairs guarantee it
            // can join before this supervisor returns, so the share port can
            // never outlive the reported stopped state.
            let proxy_result = flatten_task_result(proxy.await);
            let server_result = await_server_with_timeout(&mut server).await;
            proxy_result.and(server_result)
        }
    }
}

async fn await_server_with_timeout(task: &mut JoinHandle<io::Result<()>>) -> io::Result<()> {
    match tokio::time::timeout(SERVER_SHUTDOWN_TIMEOUT, &mut *task).await {
        Ok(result) => flatten_task_result(result),
        Err(_) => {
            task.abort();
            match task.await {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "internal guest server shutdown timed out",
                )),
                Err(error) => Err(io::Error::other(error)),
            }
        }
    }
}

fn flatten_task_result(result: Result<io::Result<()>, tokio::task::JoinError>) -> io::Result<()> {
    result.map_err(io::Error::other)?
}

async fn wait_for_stop(mut stop: watch::Receiver<bool>) {
    if !*stop.borrow() {
        let _change_result = stop.changed().await;
    }
}

fn signal_gateway_stop(server_stop: &watch::Sender<bool>, proxy_control: &ProxyControl) {
    proxy_control.stop();
    let _stop_result = server_stop.send(true);
}

struct ProxyControl {
    stopped: AtomicBool,
    active_connections: Arc<ActiveConnections>,
    wake_writer: StdMutex<UnixStream>,
}

impl ProxyControl {
    fn stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.active_connections.shutdown_all();
            let mut wake_writer = self
                .wake_writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _wake_result = wake_writer.write(&[1]);
        }
    }
}

fn proxy_accept_loop_blocking(
    public_listener: StdTcpListener,
    internal_addr: SocketAddr,
    wake_reader: UnixStream,
    control: Arc<ProxyControl>,
) -> io::Result<()> {
    let available_connections = Arc::new(Semaphore::new(MAX_PROXY_CONNECTIONS));
    let mut workers = Vec::new();
    let accept_result = accept_connections(
        &public_listener,
        internal_addr,
        &wake_reader,
        &control,
        &available_connections,
        &mut workers,
    );
    control.active_connections.shutdown_all();
    let worker_result = join_proxy_workers(workers);
    accept_result.and(worker_result)
}

fn accept_connections(
    public_listener: &StdTcpListener,
    internal_addr: SocketAddr,
    mut wake_reader: &UnixStream,
    control: &ProxyControl,
    available_connections: &Arc<Semaphore>,
    workers: &mut Vec<thread::JoinHandle<io::Result<()>>>,
) -> io::Result<()> {
    loop {
        reap_finished_workers(workers)?;
        if control.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut descriptors = [
            PollFd::new(public_listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(wake_reader.as_fd(), PollFlags::POLLIN),
        ];
        let ready = match poll(
            &mut descriptors,
            PollTimeout::from(PROXY_POLL_TIMEOUT_MILLIS),
        ) {
            Ok(ready) => ready,
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
        };
        if control.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        if ready == 0 {
            continue;
        }
        let listener_events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
        let wake_events = descriptors[1].revents().unwrap_or_else(PollFlags::empty);
        if !wake_events.is_empty() {
            let mut wake_byte = [0_u8; 1];
            let _read_result = wake_reader.read(&mut wake_byte);
            if control.stopped.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        if listener_events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
        {
            return Err(io::Error::other(
                "guest listener reported a terminal socket event",
            ));
        }
        if !listener_events.contains(PollFlags::POLLIN) {
            continue;
        }
        let (guest, _remote_addr) = match public_listener.accept() {
            Ok(connection) => connection,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        if control.stopped.load(Ordering::Acquire) {
            shutdown_both(&guest);
            return Ok(());
        }
        let Ok(permit) = Arc::clone(available_connections).try_acquire_owned() else {
            shutdown_both(&guest);
            continue;
        };
        let Some(registration) = control.active_connections.register(&guest)? else {
            shutdown_both(&guest);
            return Ok(());
        };
        let worker = thread::Builder::new()
            .name("blackwall-guest-proxy".to_owned())
            .spawn(move || {
                let _permit = permit;
                proxy_connection(guest, internal_addr, registration)
            })?;
        workers.push(worker);
    }
}

fn reap_finished_workers(workers: &mut Vec<thread::JoinHandle<io::Result<()>>>) -> io::Result<()> {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            join_proxy_worker(worker)?;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn join_proxy_workers(workers: Vec<thread::JoinHandle<io::Result<()>>>) -> io::Result<()> {
    for worker in workers {
        join_proxy_worker(worker)?;
    }
    Ok(())
}

fn join_proxy_worker(worker: thread::JoinHandle<io::Result<()>>) -> io::Result<()> {
    // Client disconnects and forced shutdowns are expected I/O outcomes. A
    // panic is not: surface it so the gateway supervisor marks sharing dead.
    let _connection_result = worker
        .join()
        .map_err(|_| io::Error::other("guest proxy worker panicked"))?;
    Ok(())
}

fn proxy_connection(
    mut guest: StdTcpStream,
    internal_addr: SocketAddr,
    registration: ConnectionRegistration,
) -> io::Result<()> {
    guest.set_read_timeout(Some(PROXY_IO_TIMEOUT))?;
    guest.set_write_timeout(Some(PROXY_IO_TIMEOUT))?;
    let mut internal = StdTcpStream::connect_timeout(&internal_addr, PROXY_CONNECT_TIMEOUT)?;
    internal.set_read_timeout(Some(PROXY_IO_TIMEOUT))?;
    internal.set_write_timeout(Some(PROXY_IO_TIMEOUT))?;
    registration.attach(&internal)?;
    let mut guest_reader = guest.try_clone()?;
    let mut internal_writer = internal.try_clone()?;
    let upload = std::thread::spawn(move || {
        let result = io::copy(&mut guest_reader, &mut internal_writer);
        let _shutdown_result = internal_writer.shutdown(Shutdown::Write);
        result
    });

    let download_result = io::copy(&mut internal, &mut guest);
    let _shutdown_result = guest.shutdown(Shutdown::Write);
    let upload_result = upload
        .join()
        .map_err(|_| io::Error::other("guest upload proxy thread panicked"))?;
    upload_result?;
    download_result?;
    Ok(())
}

#[derive(Default)]
struct ActiveConnections {
    next_id: AtomicU64,
    state: StdMutex<ActiveConnectionState>,
}

#[derive(Default)]
struct ActiveConnectionState {
    stopped: bool,
    streams: Vec<(u64, Vec<StdTcpStream>)>,
}

impl ActiveConnections {
    fn register(
        self: &Arc<Self>,
        stream: &StdTcpStream,
    ) -> io::Result<Option<ConnectionRegistration>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let retained_stream = stream.try_clone()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopped {
            shutdown_both(&retained_stream);
            return Ok(None);
        }
        state.streams.push((id, vec![retained_stream]));
        Ok(Some(ConnectionRegistration {
            id,
            connections: Arc::clone(self),
        }))
    }

    fn attach(&self, id: u64, stream: &StdTcpStream) -> io::Result<()> {
        let retained_stream = stream.try_clone()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let streams = if state.stopped {
            None
        } else {
            state
                .streams
                .iter_mut()
                .find_map(|(registered_id, streams)| (*registered_id == id).then_some(streams))
        };
        let Some(streams) = streams else {
            shutdown_both(&retained_stream);
            shutdown_both(stream);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "guest proxy stopped before the internal connection was registered",
            ));
        };
        streams.push(retained_stream);
        Ok(())
    }

    fn shutdown_all(&self) {
        let streams = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopped = true;
            std::mem::take(&mut state.streams)
        };
        for (_, connection_streams) in streams {
            for stream in connection_streams {
                shutdown_both(&stream);
            }
        }
    }

    fn unregister(&self, id: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .streams
            .retain(|(registered_id, _)| *registered_id != id);
    }
}

struct ConnectionRegistration {
    id: u64,
    connections: Arc<ActiveConnections>,
}

impl ConnectionRegistration {
    fn attach(&self, stream: &StdTcpStream) -> io::Result<()> {
        self.connections.attach(self.id, stream)
    }
}

impl Drop for ConnectionRegistration {
    fn drop(&mut self) {
        self.connections.unregister(self.id);
    }
}

fn shutdown_both(stream: &StdTcpStream) {
    loop {
        match stream.shutdown(Shutdown::Both) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(()) | Err(_) => return,
        }
    }
}

async fn verify_listener_ready(local_addr: SocketAddr) -> io::Result<()> {
    tokio::task::spawn_blocking(move || verify_listener_ready_blocking(local_addr))
        .await
        .map_err(io::Error::other)?
}

fn verify_listener_ready_blocking(local_addr: SocketAddr) -> io::Result<()> {
    let deadline = Instant::now() + LISTENER_STARTUP_TIMEOUT;
    let mut last_error =
        io::Error::new(io::ErrorKind::TimedOut, "guest listener startup timed out");
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt_timeout = remaining.min(STARTUP_ATTEMPT_TIMEOUT);
        match verify_listener_ready_attempt(local_addr, attempt_timeout) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
        thread::sleep(STARTUP_RETRY_DELAY.min(deadline.saturating_duration_since(Instant::now())));
    }
    Err(io::Error::new(
        last_error.kind(),
        format!("guest listener startup failed: {last_error}"),
    ))
}

fn verify_listener_ready_attempt(local_addr: SocketAddr, timeout: Duration) -> io::Result<()> {
    let mut stream = StdTcpStream::connect_timeout(&local_addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: blackwall.local\r\nConnection: close\r\n\r\n")?;
    let mut prefix = [0_u8; 12];
    stream.read_exact(&mut prefix)?;
    if &prefix == b"HTTP/1.1 200" {
        Ok(())
    } else {
        Err(io::Error::other(
            "guest listener health check returned an unexpected response",
        ))
    }
}

#[cfg(test)]
fn fetch_http_blocking(local_addr: SocketAddr, path: &str) -> io::Result<Vec<u8>> {
    let mut stream = StdTcpStream::connect_timeout(&local_addr, LISTENER_STARTUP_TIMEOUT)?;
    stream.set_read_timeout(Some(LISTENER_STARTUP_TIMEOUT))?;
    stream.set_write_timeout(Some(LISTENER_STARTUP_TIMEOUT))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: blackwall.local\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn router(state: Arc<GatewayState>) -> Router {
    router_with_body_limit(state, MAX_REQUEST_BYTES)
}

fn router_with_body_limit(state: Arc<GatewayState>, max_request_bytes: usize) -> Router {
    Router::new()
        .route("/", get(guest_redirect))
        .route("/guest", get(guest_page))
        .route("/guest/style.css", get(guest_css))
        .route("/guest/app.js", get(guest_js))
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(max_request_bytes))
        .with_state(state)
}

async fn guest_redirect() -> impl IntoResponse {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [
            ("location", "/guest"),
            (CACHE_CONTROL.as_str(), "no-store"),
            (REFERRER_POLICY.as_str(), "no-referrer"),
        ],
    )
}

async fn guest_page() -> Response {
    static_response(Html(GUEST_HTML), "text/html; charset=utf-8")
}

async fn guest_css() -> Response {
    static_response(GUEST_CSS, "text/css; charset=utf-8")
}

async fn guest_js() -> Response {
    static_response(GUEST_JS, "text/javascript; charset=utf-8")
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

async fn health(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    let active =
        !state.revoked.load(Ordering::Acquire) && unix_epoch_millis() < state.expires_at_ms;
    (
        [(CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(json!({ "status": if active { "ok" } else { "inactive" } })),
    )
}

struct GuestAccess(Arc<GatewayState>);

impl FromRequestParts<Arc<GatewayState>> for GuestAccess {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<GatewayState>,
    ) -> Result<Self, Self::Rejection> {
        authorize(state, &parts.headers)?;
        Ok(Self(Arc::clone(state)))
    }
}

struct ChatAccess {
    state: Arc<GatewayState>,
    permit: OwnedSemaphorePermit,
}

impl FromRequestParts<Arc<GatewayState>> for ChatAccess {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<GatewayState>,
    ) -> Result<Self, Self::Rejection> {
        authorize(state, &parts.headers)?;
        let permit = Arc::clone(&state.in_flight)
            .try_acquire_owned()
            .map_err(|_| {
                ApiError::too_many_requests(
                    "The host is handling two guest requests. Try again shortly.",
                )
            })?;
        Ok(Self {
            state: Arc::clone(state),
            permit,
        })
    }
}

async fn models(GuestAccess(state): GuestAccess) -> Result<Response, ApiError> {
    state.request_count.fetch_add(1, Ordering::Relaxed);
    let body = Json(json!({
        "object": "list",
        "data": [{
            "id": state.pinned_model,
            "object": "model",
            "created": 0,
            "owned_by": "blackwall-host"
        }]
    }));
    Ok(no_store(body.into_response()))
}

async fn chat_completions(
    ChatAccess { state, permit }: ChatAccess,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = body.map_err(|rejection| {
        let message = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            "The chat request exceeded the 32 MB safety limit."
        } else {
            "The chat request body could not be read."
        };
        ApiError::new(rejection.status(), message)
    })?;
    let mut payload: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("The chat request must be valid JSON."))?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| ApiError::bad_request("The chat request must be a JSON object."))?;
    object.insert(
        "model".to_owned(),
        Value::String(state.pinned_model.clone()),
    );

    state.request_count.fetch_add(1, Ordering::Relaxed);
    let mut request = state
        .client
        .post(format!("{}/chat/completions", state.upstream_endpoint))
        .header(CONTENT_TYPE, "application/json")
        .json(&payload);
    if let Some(api_key) = state.upstream_api_key.as_deref() {
        request = request.bearer_auth(api_key);
    }
    let upstream = tokio::time::timeout(UPSTREAM_HEADER_TIMEOUT, request.send())
        .await
        .map_err(|_| ApiError::bad_gateway("The host model did not begin responding in time."))?
        .map_err(|_error| ApiError::bad_gateway("The host model is unavailable."))?;
    let status = upstream.status();
    if !status.is_success() {
        let _bounded_error_body = read_upstream_error_bounded(upstream).await;
        return Err(ApiError::upstream(
            status,
            format!("The host model rejected the request (HTTP {status})."),
        ));
    }
    if upstream
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ApiError::bad_gateway(
            "The host model response was too large.",
        ));
    }

    let content_type = upstream.headers().get(CONTENT_TYPE).cloned();
    let stream = bounded_body_stream(upstream, permit);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response.headers_mut().insert(CONTENT_TYPE, content_type);
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

async fn read_upstream_error_bounded(response: reqwest::Response) -> String {
    let mut stream = response.bytes_stream();
    let mut collected = Vec::with_capacity(MAX_UPSTREAM_ERROR_BYTES.min(1_024));
    while collected.len() < MAX_UPSTREAM_ERROR_BYTES {
        let next = match tokio::time::timeout(RESPONSE_IDLE_TIMEOUT, stream.next()).await {
            Ok(next) => next,
            Err(_) => break,
        };
        let Some(Ok(chunk)) = next else { break };
        let remaining = MAX_UPSTREAM_ERROR_BYTES.saturating_sub(collected.len());
        collected.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    String::from_utf8_lossy(&collected).into_owned()
}

fn bounded_body_stream(
    response: reqwest::Response,
    permit: OwnedSemaphorePermit,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + 'static {
    async_stream::try_stream! {
        let _permit = permit;
        let mut stream = response.bytes_stream();
        let mut received = 0_usize;
        loop {
            let next = tokio::time::timeout(RESPONSE_IDLE_TIMEOUT, stream.next())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "upstream response became idle"))?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(io::Error::other)?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                Err(io::Error::other("upstream response exceeded the safety limit"))?;
            }
            yield chunk;
        }
    }
}

fn authorize(state: &GatewayState, headers: &HeaderMap) -> Result<(), ApiError> {
    if state.revoked.load(Ordering::Acquire) {
        return Err(ApiError::unauthorized(
            "This invite was revoked by the host.",
        ));
    }
    if unix_epoch_millis() >= state.expires_at_ms {
        return Err(ApiError::unauthorized("This invite has expired."));
    }

    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    let presented_hash = salted_key_hash(&state.salt, presented);
    if presented_hash.ct_eq(&state.key_hash).unwrap_u8() != 1 {
        return Err(ApiError::unauthorized("The invite key is invalid."));
    }
    Ok(())
}

pub(crate) fn salted_key_hash(salt: &[u8; 32], key: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(key.as_bytes());
    hasher.finalize().into()
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

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "Route not found.")
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
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

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn upstream(status: reqwest::StatusCode, message: impl Into<String>) -> Self {
        let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        Self::new(status, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let response = (
            self.status,
            Json(json!({
                "error": {
                    "message": self.message,
                    "type": "blackwall_share_error"
                }
            })),
        )
            .into_response();
        no_store(response)
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
    use super::*;
    use axum::{
        body::to_bytes,
        http::{Method, Request},
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use tokio::net::TcpListener as TokioTcpListener;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    static LISTENER_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    fn test_state(upstream_endpoint: String, expires_at_ms: u64) -> (Arc<GatewayState>, String) {
        let key = format!("bw1_{}", URL_SAFE_NO_PAD.encode([7_u8; 32]));
        let salt = [9_u8; 32];
        let key_hash = salted_key_hash(&salt, &key);
        (
            Arc::new(GatewayState {
                client: reqwest::Client::new(),
                upstream_endpoint,
                upstream_api_key: None,
                pinned_model: "host-pinned-model".to_owned(),
                salt,
                key_hash,
                expires_at_ms,
                revoked: AtomicBool::new(false),
                request_count: AtomicU64::new(0),
                in_flight: Arc::new(Semaphore::new(2)),
            }),
            key,
        )
    }

    fn api_request(method: Method, uri: &str, key: Option<&str>, body: Body) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(key) = key {
            builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        builder
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
    }

    fn observed_body(polled: Arc<AtomicBool>) -> Body {
        Body::from_stream(futures_util::stream::poll_fn(move |_context| {
            polled.store(true, Ordering::Release);
            std::task::Poll::Ready(None::<Result<Bytes, io::Error>>)
        }))
    }

    #[test]
    fn salted_hash_is_deterministic_and_salt_specific() {
        let key = format!("bw1_{}", URL_SAFE_NO_PAD.encode([7_u8; 32]));
        let first = salted_key_hash(&[1; 32], &key);
        assert_eq!(first, salted_key_hash(&[1; 32], &key));
        assert_ne!(first, salted_key_hash(&[2; 32], &key));
        assert_ne!(first, salted_key_hash(&[1; 32], "bw1_wrong"));
    }

    #[test]
    fn guest_page_has_external_assets_and_no_inline_script() {
        assert!(GUEST_HTML.contains("src=\"/guest/app.js\""));
        assert!(GUEST_HTML.contains("href=\"/guest/style.css\""));
        assert!(!GUEST_HTML.contains("<script>"));
        assert!(GUEST_JS.contains("history.replaceState"));
        assert!(CONTENT_SECURITY_POLICY_VALUE.contains("default-src 'none'"));
    }

    #[tokio::test]
    async fn authentication_accepts_valid_key_and_rejects_invalid_expired_and_revoked_keys() {
        let (state, key) = test_state(
            "http://127.0.0.1:1/v1".to_owned(),
            unix_epoch_millis() + 60_000,
        );
        let app = router(Arc::clone(&state));

        let valid = app
            .clone()
            .oneshot(api_request(
                Method::GET,
                "/v1/models",
                Some(&key),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(valid.status(), StatusCode::OK);
        let valid_body = to_bytes(valid.into_body(), 32 * 1024).await.unwrap();
        let valid_json: Value = serde_json::from_slice(&valid_body).unwrap();
        assert_eq!(valid_json["data"][0]["id"], "host-pinned-model");

        let invalid = app
            .clone()
            .oneshot(api_request(
                Method::GET,
                "/v1/models",
                Some("bw1_not-the-key"),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);

        let missing = app
            .clone()
            .oneshot(api_request(Method::GET, "/v1/models", None, Body::empty()))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        state.revoked.store(true, Ordering::Release);
        let revoked = app
            .oneshot(api_request(
                Method::GET,
                "/v1/models",
                Some(&key),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        let revoked_body = to_bytes(revoked.into_body(), 32 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&revoked_body).contains("revoked"));

        let (expired_state, expired_key) =
            test_state("http://127.0.0.1:1/v1".to_owned(), unix_epoch_millis());
        let expired = router(expired_state)
            .oneshot(api_request(
                Method::GET,
                "/v1/models",
                Some(&expired_key),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
        let expired_body = to_bytes(expired.into_body(), 32 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&expired_body).contains("expired"));
    }

    #[tokio::test]
    async fn authentication_and_concurrency_rejections_do_not_poll_the_request_body() {
        let (state, key) = test_state(
            "http://127.0.0.1:1/v1".to_owned(),
            unix_epoch_millis() + 60_000,
        );
        let app = router(Arc::clone(&state));

        let unauthorized_body_polled = Arc::new(AtomicBool::new(false));
        let unauthorized = app
            .clone()
            .oneshot(api_request(
                Method::POST,
                "/v1/chat/completions",
                None,
                observed_body(Arc::clone(&unauthorized_body_polled)),
            ))
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert!(!unauthorized_body_polled.load(Ordering::Acquire));

        let first = Arc::clone(&state.in_flight).try_acquire_owned().unwrap();
        let second = Arc::clone(&state.in_flight).try_acquire_owned().unwrap();
        let busy_body_polled = Arc::new(AtomicBool::new(false));
        let busy = app
            .oneshot(api_request(
                Method::POST,
                "/v1/chat/completions",
                Some(&key),
                observed_body(Arc::clone(&busy_body_polled)),
            ))
            .await
            .unwrap();
        assert_eq!(busy.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!busy_body_polled.load(Ordering::Acquire));
        drop((first, second));
    }

    #[tokio::test]
    async fn body_limit_rejections_keep_the_json_api_error_contract() {
        let (state, key) = test_state(
            "http://127.0.0.1:1/v1".to_owned(),
            unix_epoch_millis() + 60_000,
        );
        let response = router_with_body_limit(state, 8)
            .oneshot(api_request(
                Method::POST,
                "/v1/chat/completions",
                Some(&key),
                Body::from(r#"{"messages":[]}"#),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = to_bytes(response.into_body(), 32 * 1024).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"]["type"], "blackwall_share_error");
        assert!(payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("32 MB"));
    }

    #[derive(Clone, Default)]
    struct CannedUpstream {
        last_payload: Arc<Mutex<Option<Value>>>,
    }

    async fn canned_chat(
        State(state): State<CannedUpstream>,
        Json(payload): Json<Value>,
    ) -> Response {
        *state.last_payload.lock().await = Some(payload);
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\" guest\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            )))
            .unwrap()
    }

    #[tokio::test]
    async fn proxy_pins_model_and_streams_sse_through_to_completion() {
        let canned = CannedUpstream::default();
        let upstream_router = Router::new()
            .route("/v1/chat/completions", post(canned_chat))
            .with_state(canned.clone());
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream_router).await.unwrap();
        });

        let (state, key) = test_state(
            format!("http://{upstream_addr}/v1"),
            unix_epoch_millis() + 60_000,
        );
        let request_body = serde_json::json!({
            "model": "guest-tried-to-select-this",
            "stream": true,
            "messages": [{ "role": "user", "content": "Hello" }]
        });
        let response = router(Arc::clone(&state))
            .oneshot(api_request(
                Method::POST,
                "/v1/chat/completions",
                Some(&key),
                Body::from(serde_json::to_vec(&request_body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let stream = String::from_utf8(body.to_vec()).unwrap();
        assert!(stream.contains("Hello"));
        assert!(stream.contains(" guest"));
        assert!(stream.ends_with("data: [DONE]\n\n"));

        let upstream_payload = canned.last_payload.lock().await.clone().unwrap();
        assert_eq!(upstream_payload["model"], "host-pinned-model");
        assert_eq!(state.request_count.load(Ordering::Relaxed), 1);
        upstream_task.abort();
    }

    #[tokio::test]
    async fn spawned_listener_serves_static_http_after_start_returns() {
        let _listener_guard = LISTENER_TEST_LOCK.lock().await;
        let key = format!("bw1_{}", URL_SAFE_NO_PAD.encode([11_u8; 32]));
        let salt = [12_u8; 32];
        let detected_network = crate::share::tailscale::detect_share_network();
        let running = start_gateway(GatewayConfig {
            client: reqwest::Client::new(),
            upstream_endpoint: "http://127.0.0.1:1/v1".to_owned(),
            upstream_api_key: None,
            pinned_model: "host-pinned-model".to_owned(),
            bind_ip: detected_network.bind_ip,
            port: 0,
            salt,
            key_hash: salted_key_hash(&salt, &key),
            expires_at_ms: unix_epoch_millis() + 60_000,
        })
        .await
        .unwrap();

        let address = running.local_addr;
        let response = tokio::task::spawn_blocking(move || fetch_http_blocking(address, "/guest"))
            .await
            .unwrap()
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response
            .to_ascii_lowercase()
            .contains("content-security-policy:"));
        assert!(response.contains(CONTENT_SECURITY_POLICY_VALUE));
        assert!(response.contains("Blackwall Guest"));

        running.control.revoke();
        let _shutdown_result = running.shutdown.send(());
        tokio::time::timeout(Duration::from_secs(2), running.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_closes_an_accepted_keep_alive_connection_and_listener() {
        let _listener_guard = LISTENER_TEST_LOCK.lock().await;
        let key = format!("bw1_{}", URL_SAFE_NO_PAD.encode([21_u8; 32]));
        let salt = [22_u8; 32];
        let running = start_gateway(GatewayConfig {
            client: reqwest::Client::new(),
            upstream_endpoint: "http://127.0.0.1:1/v1".to_owned(),
            upstream_api_key: None,
            pinned_model: "host-pinned-model".to_owned(),
            bind_ip: Ipv4Addr::LOCALHOST,
            port: 0,
            salt,
            key_hash: salted_key_hash(&salt, &key),
            expires_at_ms: unix_epoch_millis() + 60_000,
        })
        .await
        .unwrap();

        let address = running.local_addr;
        let mut idle = tokio::task::spawn_blocking(move || -> io::Result<StdTcpStream> {
            let mut stream = StdTcpStream::connect_timeout(&address, Duration::from_secs(2))?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            stream.write_all(
                b"GET /health HTTP/1.1\r\nHost: blackwall.local\r\nConnection: keep-alive\r\n\r\n",
            )?;
            let mut response = Vec::new();
            let mut chunk = [0_u8; 512];
            while !response
                .windows(b"{\"status\":\"ok\"}".len())
                .any(|window| window == b"{\"status\":\"ok\"}")
            {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "keep-alive health response ended early",
                    ));
                }
                response.extend_from_slice(&chunk[..read]);
            }
            Ok(stream)
        })
        .await
        .unwrap()
        .unwrap();

        running.control.revoke();
        let _shutdown_result = running.shutdown.send(());
        tokio::time::timeout(Duration::from_secs(4), running.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        idle.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut byte = [0_u8; 1];
        assert!(matches!(idle.read(&mut byte), Ok(0) | Err(_)));
        assert!(StdTcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err());
    }

    #[tokio::test]
    async fn third_concurrent_chat_is_rejected_before_upstream_work() {
        let (state, key) = test_state(
            "http://127.0.0.1:1/v1".to_owned(),
            unix_epoch_millis() + 60_000,
        );
        let first = Arc::clone(&state.in_flight).try_acquire_owned().unwrap();
        let second = Arc::clone(&state.in_flight).try_acquire_owned().unwrap();
        let response = router(state)
            .oneshot(api_request(
                Method::POST,
                "/v1/chat/completions",
                Some(&key),
                Body::from(r#"{"messages":[{"role":"user","content":"hi"}]}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        drop((first, second));
    }
}
