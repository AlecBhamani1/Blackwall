#![allow(clippy::unwrap_used)]
use axum::{routing::post, Router};
use blackwall_core::{pairing::*, share::ShareHub};
use blackwall_relay::{app, RelayConfig};
use reqwest::{Client, Method};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::TcpListener;

async fn call(
    client: &Client,
    base: &str,
    path: &str,
    method: Method,
    key: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = client
        .request(method, format!("{base}/v1/pairing{path}"))
        .bearer_auth(key)
        .header("x-blackwall-relay-token", "test-operator");
    if let Some(body) = body {
        request = request.json(&body);
    }
    request.send().await.unwrap()
}
async fn offer(client: &Client, base: &str) -> (PairingOffer, String, String) {
    let invitation = secret("bwi_");
    let host = secret("bwh_");
    let (salt, hash) = digest(&invitation);
    let offer = PairingOffer {
        version: 1,
        id: secret("bwp_"),
        host_name: "Home Mac".into(),
        model: "paired-model".into(),
        salt,
        hash,
    };
    assert!(
        call(client, base, "", Method::POST, &host, Some(json!(offer)))
            .await
            .status()
            .is_success()
    );
    (offer, host, invitation)
}
fn candidate() -> (PairingCandidate, String) {
    let key = secret("bw1_");
    let (salt, hash) = digest(&key);
    (
        PairingCandidate {
            id: secret("bwd_"),
            name: "My laptop".into(),
            salt,
            hash,
        },
        key,
    )
}
async fn server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app(RelayConfig {
                registration_token: Some("test-operator".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    });
    (base, task)
}
#[tokio::test]
async fn pairing_requires_exact_host_consent_and_exchange_cannot_be_reused() {
    let (base, server) = server().await;
    let client = Client::new();
    let (offer, host, invitation) = offer(&client, &base).await;
    let (candidate, key) = candidate();
    let join = format!("/{}/join", offer.id);
    let progress: PairingProgress = call(
        &client,
        &base,
        &join,
        Method::POST,
        &invitation,
        Some(json!(candidate)),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(progress.state, "review");
    assert_eq!(
        progress.confirmation,
        Some(confirmation(&offer.id, &candidate))
    );
    assert!(call(
        &client,
        &base,
        &join,
        Method::POST,
        &invitation,
        Some(json!(candidate))
    )
    .await
    .status()
    .is_success());
    let (other, _) = self::candidate();
    assert!(call(
        &client,
        &base,
        &join,
        Method::POST,
        &invitation,
        Some(json!(other))
    )
    .await
    .status()
    .is_client_error());
    let approval = PairingApproval {
        candidate: candidate.clone(),
        session_id: secret("bws_"),
    };
    assert_eq!(
        client
            .get(format!("{base}/s/{}/v1/models", approval.session_id))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let approve = format!("/{}/approve", offer.id);
    assert_eq!(
        call(
            &client,
            &base,
            &approve,
            Method::POST,
            &invitation,
            Some(json!(approval))
        )
        .await
        .status(),
        401
    );
    let stale = PairingApproval {
        candidate: other,
        session_id: approval.session_id.clone(),
    };
    assert!(call(
        &client,
        &base,
        &approve,
        Method::POST,
        &host,
        Some(json!(stale))
    )
    .await
    .status()
    .is_client_error());
    assert!(call(
        &client,
        &base,
        &approve,
        Method::POST,
        &host,
        Some(json!(approval))
    )
    .await
    .status()
    .is_success());
    assert!(call(
        &client,
        &base,
        &approve,
        Method::POST,
        &host,
        Some(json!(approval))
    )
    .await
    .status()
    .is_success());
    let changed = PairingApproval {
        candidate: candidate.clone(),
        session_id: secret("bws_"),
    };
    assert!(call(
        &client,
        &base,
        &approve,
        Method::POST,
        &host,
        Some(json!(changed))
    )
    .await
    .status()
    .is_client_error());
    assert!(call(
        &client,
        &base,
        &join,
        Method::POST,
        &invitation,
        Some(json!(candidate))
    )
    .await
    .status()
    .is_client_error());
    let result = format!("/{}/result", offer.id);
    assert_eq!(
        call(&client, &base, &result, Method::GET, &invitation, None)
            .await
            .status(),
        401
    );
    let receipt: PairingProgress = call(&client, &base, &result, Method::GET, &key, None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(receipt.state, "approved");
    assert_eq!(receipt.session_id, Some(approval.session_id));
    server.abort();
}
#[tokio::test]
async fn cancelled_and_guessed_invitations_never_gain_access() {
    let (base, server) = server().await;
    let client = Client::new();
    let (offer, _host, invitation) = offer(&client, &base).await;
    let (candidate, _) = candidate();
    let join = format!("/{}/join", offer.id);
    for _ in 0..5 {
        assert_eq!(
            call(
                &client,
                &base,
                &join,
                Method::POST,
                "wrong-key",
                Some(json!(candidate))
            )
            .await
            .status(),
            401
        );
    }
    assert_eq!(
        call(
            &client,
            &base,
            &join,
            Method::POST,
            &invitation,
            Some(json!(candidate))
        )
        .await
        .status(),
        429
    );
    let (offer, host, invitation) = self::offer(&client, &base).await;
    let join = format!("/{}/join", offer.id);
    assert!(call(
        &client,
        &base,
        &join,
        Method::POST,
        &invitation,
        Some(json!(candidate))
    )
    .await
    .status()
    .is_success());
    assert!(call(
        &client,
        &base,
        &format!("/{}/host", offer.id),
        Method::DELETE,
        &host,
        None
    )
    .await
    .status()
    .is_success());
    let approval = PairingApproval {
        candidate,
        session_id: secret("bws_"),
    };
    assert!(call(
        &client,
        &base,
        &format!("/{}/approve", offer.id),
        Method::POST,
        &host,
        Some(json!(approval))
    )
    .await
    .status()
    .is_client_error());
    assert!(call(
        &client,
        &base,
        &format!("/devices/{}", secret("bws_")),
        Method::DELETE,
        &invitation,
        None
    )
    .await
    .status()
    .is_client_error());
    server.abort();
}
#[tokio::test]
async fn candidate_can_cancel_without_allowing_stale_host_approval() {
    let (base, server) = server().await;
    let client = Client::new();
    let (offer, host, invitation) = offer(&client, &base).await;
    let (candidate, key) = candidate();
    let join = format!("/{}/join", offer.id);
    assert!(call(
        &client,
        &base,
        &join,
        Method::POST,
        &invitation,
        Some(json!(candidate))
    )
    .await
    .status()
    .is_success());
    let result = format!("/{}/result", offer.id);
    assert_eq!(
        call(&client, &base, &result, Method::DELETE, &invitation, None)
            .await
            .status(),
        401
    );
    for _ in 0..2 {
        assert!(call(&client, &base, &result, Method::DELETE, &key, None)
            .await
            .status()
            .is_success());
    }
    let approval = PairingApproval {
        candidate,
        session_id: secret("bws_"),
    };
    assert!(call(
        &client,
        &base,
        &format!("/{}/approve", offer.id),
        Method::POST,
        &host,
        Some(json!(approval))
    )
    .await
    .status()
    .is_client_error());
    let status: PairingProgress = call(
        &client,
        &base,
        &format!("/{}/host", offer.id),
        Method::GET,
        &host,
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(status.state, "denied");
    server.abort();
}
#[tokio::test]
async fn saved_device_relaunch_and_revocation_preserve_other_devices_and_streams() {
    let (base, server) = server().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/v1", listener.local_addr().unwrap());
    let held = std::sync::Arc::new(tokio::sync::Notify::new());
    let notify = held.clone();
    let router = Router::new().route("/v1/chat/completions", post(move |axum::Json(body): axum::Json<Value>| {
        let notify = notify.clone();
        async move {
        if body["messages"][0]["content"] == "hold" { notify.notify_one(); std::future::pending::<()>().await; }
 ([("content-type", "text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"paired answer\"}}]}\n\ndata: [DONE]\n\n") } }));
    let model = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let directory = std::env::temp_dir().join(format!(
        "blackwall-pair-roundtrip-{}",
        rand::random::<u64>()
    ));
    let mut saved = Vec::new();
    let mut hubs = Vec::new();
    for _ in 0..2 {
        let (offer, host, invitation) = offer(&client, &base).await;
        let (candidate, key) = candidate();
        assert!(call(
            &client,
            &base,
            &format!("/{}/join", offer.id),
            Method::POST,
            &invitation,
            Some(json!(candidate))
        )
        .await
        .status()
        .is_success());
        let device = PairedDevice {
            id: secret("bws_"),
            role: "host".into(),
            name: candidate.name.clone(),
            relay_url: base.clone(),
            model: "paired-model".into(),
            upstream: Some(upstream.clone()),
            salt: candidate.salt.clone(),
            hash: candidate.hash.clone(),
            state: "active".into(),
        };
        let approval = PairingApproval {
            candidate,
            session_id: device.id.clone(),
        };
        blackwall_core::storage::LocalStore::open(&directory)
            .unwrap()
            .save_paired_device(&device)
            .unwrap();
        assert!(call(
            &client,
            &base,
            &format!("/{}/approve", offer.id),
            Method::POST,
            &host,
            Some(json!(approval))
        )
        .await
        .status()
        .is_success());
        let hub = ShareHub::new();
        hub.start_paired(&device, host.clone(), None, Some("test-operator".into()))
            .await
            .unwrap();
        saved.push((device, host, key));
        hubs.push(hub);
    }
    async fn available(client: &Client, endpoint: &str, key: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if client
                    .get(format!("{endpoint}/models"))
                    .bearer_auth(key)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
    for (device, _, key) in &saved {
        available(&client, &device.endpoint(), key).await;
    }
    assert_eq!(
        client
            .get(format!("{}/models", saved[1].0.endpoint()))
            .bearer_auth(&saved[0].2)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    hubs[0].stop().await;
    let restored = blackwall_core::storage::LocalStore::open(&directory)
        .unwrap()
        .paired_devices()
        .unwrap()
        .into_iter()
        .find(|device| device.id == saved[0].0.id)
        .unwrap();
    hubs[0]
        .start_paired(
            &restored,
            saved[0].1.clone(),
            None,
            Some("test-operator".into()),
        )
        .await
        .unwrap();
    available(&client, &restored.endpoint(), &saved[0].2).await;
    let answer = client
        .post(format!("{}/chat/completions", restored.endpoint()))
        .bearer_auth(&saved[0].2)
        .json(&json!({"messages":[], "stream":true}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(answer.contains("paired answer"));
    let waiting = client
        .post(format!("{}/chat/completions", restored.endpoint()))
        .bearer_auth(&saved[0].2)
        .json(&json!({"messages":[{"role":"user","content":"hold"}],"stream":true}));
    let pending = tokio::spawn(async move { waiting.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(3), held.notified())
        .await
        .unwrap();
    assert!(call(
        &client,
        &base,
        &format!("/devices/{}", restored.id),
        Method::DELETE,
        &saved[0].1,
        None
    )
    .await
    .status()
    .is_success());
    assert!(!client
        .get(format!("{}/models", restored.endpoint()))
        .bearer_auth(&saved[0].2)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let ended = pending.await.unwrap();
    assert_eq!(ended.status(), 502);
    assert!(ended.text().await.unwrap().contains("removed by its owner"));
    available(&client, &saved[1].0.endpoint(), &saved[1].2).await;
    // A stale host with the original credential cannot undo durable revocation.
    hubs[0].stop().await;
    hubs[0]
        .start_paired(
            &restored,
            saved[0].1.clone(),
            None,
            Some("test-operator".into()),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!client
        .get(format!("{}/models", restored.endpoint()))
        .bearer_auth(&saved[0].2)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    for hub in hubs {
        hub.stop().await;
    }
    let bytes = std::fs::read(directory.join("blackwall.sqlite3")).unwrap();
    for (_, host, client) in saved {
        assert!(!bytes
            .windows(host.len())
            .any(|window| window == host.as_bytes()));
        assert!(!bytes
            .windows(client.len())
            .any(|window| window == client.as_bytes()));
    }
    std::fs::remove_dir_all(directory).unwrap();
    server.abort();
    model.abort();
}
