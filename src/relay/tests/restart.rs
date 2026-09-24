//! Exercise the actual executable, including durable state and abrupt process death.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use blackwall_core::share::{
    relay_protocol::{HostToRelay, RelayRegistration, RelayToHost},
    salted_key_hash, ShareHub, StartShareRequest,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

struct RelayProcess {
    child: Option<Child>,
    directory: PathBuf,
    address: std::net::SocketAddr,
}
impl RelayProcess {
    fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        Self {
            child: None,
            directory: std::env::temp_dir()
                .join(format!("blackwall-restart-{}", rand::random::<u64>())),
            address,
        }
    }
    async fn start(&mut self) {
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_blackwall-relay"))
                .env("BLACKWALL_RELAY_BIND", self.address.to_string())
                .env("BLACKWALL_RELAY_DATA_DIR", &self.directory)
                .env("BLACKWALL_RELAY_TOKEN", "restart-test-registration")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(250))
            .build()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "relay startup failed"
                );
                if client
                    .get(format!("http://{}/health", self.address))
                    .send()
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("relay must start");
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}
impl Drop for RelayProcess {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn register(relay: &RelayProcess, registration: RelayRegistration) -> (Socket, RelayToHost) {
    let (mut socket, _) = connect_async(format!("ws://{}/v1/host/connect", relay.address))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&HostToRelay::Register { registration })
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                break serde_json::from_str(&text).unwrap();
            }
        }
    })
    .await
    .unwrap();
    (socket, reply)
}
fn registration() -> RelayRegistration {
    RelayRegistration {
        protocol_version: 1,
        session_id: format!("bws_{}", URL_SAFE_NO_PAD.encode([1; 32])),
        host_key: format!("bwh_{}", URL_SAFE_NO_PAD.encode([2; 32])),
        relay_token: Some("restart-test-registration".into()),
        model: "restart-model".into(),
        guest_key_salt: URL_SAFE_NO_PAD.encode([3; 32]),
        guest_key_hash: URL_SAFE_NO_PAD.encode(salted_key_hash(&[3; 32], "guest-secret")),
        expires_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000,
    }
}

#[tokio::test]
async fn offline_address_cannot_be_taken_over_before_or_after_process_restart() {
    let mut relay = RelayProcess::new();
    relay.start().await;
    let owner = registration();
    let (mut host, reply) = register(&relay, owner.clone()).await;
    assert_eq!(reply, RelayToHost::Registered);
    host.close(None).await.unwrap();
    // Wait until the host connection is actually gone before trying the stolen public identifier.
    let client = reqwest::Client::new();
    let models = format!("{}/s/{}/v1/models", relay.url(), owner.session_id);
    tokio::time::timeout(Duration::from_secs(5), async {
        while client
            .get(&models)
            .bearer_auth("guest-secret")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut impostor = owner.clone();
    impostor.host_key = format!("bwh_{}", URL_SAFE_NO_PAD.encode([4; 32]));
    assert!(matches!(
        register(&relay, impostor.clone()).await.1,
        RelayToHost::Error { .. }
    ));
    relay.stop();
    relay.start().await;
    assert!(matches!(
        register(&relay, impostor).await.1,
        RelayToHost::Error { .. }
    ));
    let (_host, reply) = register(&relay, owner).await;
    assert_eq!(reply, RelayToHost::Registered);
    assert_eq!(
        client
            .get(models)
            .bearer_auth("guest-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn desktop_recovers_the_same_invitation_after_relay_process_restart_without_replaying_chat() {
    use axum::{routing::post, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let router = Router::new().route("/v1/chat/completions", post(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        async { ([("content-type", "text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: [DONE]\n\n") }
    }));
    let upstream_task = tokio::spawn(async move {
        axum::serve(upstream, router).await.unwrap();
    });
    let mut relay = RelayProcess::new();
    relay.start().await;
    let hub = ShareHub::new();
    let invite = hub
        .start(StartShareRequest {
            model: "restart-model".into(),
            endpoint: Some(format!("http://{address}/v1")),
            relay_url: Some(relay.url()),
            relay_token: Some("restart-test-registration".into()),
            expires_in_minutes: 1,
        })
        .await
        .unwrap();
    let (base, key) = invite.share_url.split_once("/guest#key=").unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let chat = || {
        client.post(format!("{base}/v1/chat/completions")).bearer_auth(key).json(&serde_json::json!({"model":"ignored", "messages":[{"role":"user","content":"hello"}], "stream":true}))
    };
    assert!(chat()
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
        .contains("recovered"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    relay.stop();
    relay.start().await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if client
                .get(format!("{base}/v1/models"))
                .bearer_auth(key)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("desktop must automatically restore the original invite");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "reconnect must not replay completed prompts"
    );
    assert!(chat()
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
        .contains("recovered"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    hub.stop().await;
    // Stopping joins the local host task; the separate relay process must still
    // receive the socket close and remove its session before metadata returns 404.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = client
                .get(format!("{base}/v1/models"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap()
                .status();
            if status == reqwest::StatusCode::NOT_FOUND {
                break;
            }
            assert!(
                status.is_success() || status == reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "unexpected response while the relay processes host shutdown: {status}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("relay must remove the stopped host within five seconds");
    assert_eq!(chat().send().await.unwrap().status(), 404);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    upstream_task.abort();
}

#[tokio::test]
async fn expiry_ends_an_in_flight_request_and_rejects_the_old_invitation() {
    let mut relay = RelayProcess::new();
    relay.start().await;
    let mut owner = registration();
    owner.expires_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 750;
    let (mut host, reply) = register(&relay, owner.clone()).await;
    assert_eq!(reply, RelayToHost::Registered);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("{}/s/{}/v1", relay.url(), owner.session_id);
    let request = client
        .post(format!("{base}/chat/completions"))
        .bearer_auth("guest-secret")
        .json(&serde_json::json!({"messages":[], "stream":true}));
    let response = tokio::spawn(async move { request.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Message::Text(text) = host.next().await.unwrap().unwrap() {
                if matches!(
                    serde_json::from_str::<RelayToHost>(&text).unwrap(),
                    RelayToHost::ChatRequest { .. }
                ) {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    // Keep the socket open and deliberately leave the model request unfinished.
    let response = response.await.unwrap();
    assert_eq!(response.status(), 502);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("invitation expired"));
    assert!(!client
        .get(format!("{base}/models"))
        .bearer_auth("guest-secret")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(matches!(
        register(&relay, owner).await.1,
        RelayToHost::Error { .. }
    ));
}

#[tokio::test]
async fn a_silent_host_is_disconnected_and_its_owner_can_reconnect() {
    let mut relay = RelayProcess::new();
    relay.start().await;
    let mut owner = registration();
    owner.expires_at_ms += 60_000;
    let (_silent_host, reply) = register(&relay, owner.clone()).await;
    assert_eq!(reply, RelayToHost::Registered);
    // Retain the TCP socket without polling it, simulating a sleeping host that sends no pong.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("{}/s/{}/v1/models", relay.url(), owner.session_id);
    let started = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(70), async {
        loop {
            if !client
                .get(&url)
                .bearer_auth("guest-secret")
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("silent TCP connections must not stay online forever");
    assert!(started.elapsed() >= blackwall_core::share::relay_protocol::PEER_IDLE_TIMEOUT);
    let (_reconnected, reply) = register(&relay, owner).await;
    assert_eq!(reply, RelayToHost::Registered);
    assert_eq!(
        client
            .get(url)
            .bearer_auth("guest-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}
