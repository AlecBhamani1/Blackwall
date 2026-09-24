//! Native pairing orchestration. Raw device/host keys never cross the IPC boundary.
mod lifecycle;
use blackwall_core::{pairing::*, share::ShareHub};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;

#[derive(Default)]
pub struct PairingState(Mutex<Inner>, std::sync::Mutex<HashMap<String, ShareHub>>);
#[derive(Default)]
struct Inner {
    host: Option<HostOffer>,
    client: Option<ClientRequest>,
    tunnels: HashMap<String, ShareHub>,
}
impl Inner {
    fn forget_removed_offer(&mut self, id: &str) {
        if self
            .host
            .as_ref()
            .is_some_and(|host| host.device_id.as_deref() == Some(id))
        {
            self.host = None;
        }
    }
}
struct HostOffer {
    relay: String,
    id: String,
    key: String,
    model: String,
    upstream: String,
    expires_at: u64,
    device_id: Option<String>,
}
struct ClientRequest {
    relay: String,
    id: String,
    key: String,
    candidate: PairingCandidate,
    expires_at: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingSnapshot {
    devices: Vec<DeviceStatus>,
    host: Option<PairingProgress>,
    client: Option<PairingProgress>,
    warnings: Vec<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    id: String,
    role: String,
    name: String,
    model: String,
    endpoint: String,
    state: String,
    online: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedInvitation {
    invitation: String,
    expires_at: u64,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| "The pairing connection could not start.".into())
}
async fn read_bounded_response(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "The response could not be read.")?;
        if bytes.len() + chunk.len() > limit {
            return Err("The response exceeded its size limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn model_key(app: &AppHandle, endpoint: &str) -> Result<Option<String>, String> {
    crate::commands::scoped_api_key(&app.state::<crate::commands::AppState>(), endpoint)
        .await
        .map_err(|error| error.to_string())
}
async fn relay_token(relay: &str) -> Result<Option<String>, String> {
    if let Some(key) = crate::credentials::relay_key(relay).await? {
        return Ok(Some(key));
    }
    if std::env::var("BLACKWALL_RELAY_URL")
        .is_ok_and(|url| blackwall_core::connection::same_origin(&url, relay))
    {
        return Ok(std::env::var("BLACKWALL_RELAY_TOKEN").ok());
    }
    Ok(None)
}
#[derive(Debug)]
struct RelayFailure {
    status: Option<u16>,
    message: String,
}
impl From<String> for RelayFailure {
    fn from(message: String) -> Self {
        Self {
            status: None,
            message,
        }
    }
}
impl From<&str> for RelayFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<RelayFailure> for String {
    fn from(error: RelayFailure) -> Self {
        error.message
    }
}
impl RelayFailure {
    fn definitive_rejection(&self) -> bool {
        self.status
            .is_some_and(|status| (400..500).contains(&status) && status != 408)
    }
    fn already_cancelled(&self, client: bool) -> bool {
        self.status == Some(404) || (client && self.status == Some(401))
    }
}
async fn request(
    relay: &str,
    path: &str,
    method: reqwest::Method,
    key: &str,
    body: Option<Value>,
    token: Option<String>,
) -> Result<Value, RelayFailure> {
    let mut request = http()?
        .request(method, format!("{relay}/v1/pairing{path}"))
        .bearer_auth(key);
    if let Some(token) = token {
        request = request.header("x-blackwall-relay-token", token);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|_| {
        "The pairing relay could not be reached. Check your connection and try again."
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(RelayFailure { status: Some(status.as_u16()), message: match status.as_u16() {
            401 | 403 => "Pairing access was rejected. Check the invitation or the host's relay access key.",
            404 => "This pairing invitation expired, was cancelled, or the relay restarted. Create a new invitation.",
            429 => "Pairing is busy or this invitation is locked. Wait a minute or create a new invitation.",
            _ => "This pairing request is no longer valid. Check both computers and create a new invitation.",
        }.into() });
    }
    // Never trust an unbounded or raw relay error response with display text.
    let bytes = read_bounded_response(response, 16 * 1024).await;
    let bytes = bytes.map_err(|_| "The relay sent an invalid pairing response.")?;
    serde_json::from_slice(&bytes).map_err(|_| "The relay sent an invalid pairing response.".into())
}
async fn capabilities(relay: &str) -> Result<(), String> {
    let value = request(relay, "/capabilities", reqwest::Method::GET, "", None, None).await
        .map_err(|_| "Pairing requires a reachable relay with pairing support. Update the relay and try again.")?;
    if value["version"] != PAIRING_VERSION {
        return Err("Pairing requires a relay update.".into());
    }
    Ok(())
}
async fn progress(relay: &str, id: &str, side: &str, key: &str) -> Result<PairingProgress, String> {
    let value = request(
        relay,
        &format!("/{id}/{side}"),
        reqwest::Method::GET,
        key,
        None,
        None,
    )
    .await?;
    let progress: PairingProgress =
        serde_json::from_value(value).map_err(|_| "Invalid pairing status.")?;
    validate_name(&progress.host_name)?;
    validate_model(&progress.model)?;
    if let Some(candidate) = &progress.candidate {
        validate_name(&candidate.name)?;
        if !valid_secret(&candidate.id, "bwd_")
            || decode_digest(&candidate.salt).is_none()
            || decode_digest(&candidate.hash).is_none()
            || progress.confirmation.as_deref() != Some(&confirmation(id, candidate))
        {
            return Err("The pairing confirmation could not be verified.".into());
        }
    }
    Ok(progress)
}
async fn devices(app: &AppHandle) -> Result<Vec<PairedDevice>, String> {
    crate::data::with_store(app, |store| store.paired_devices()).await
}
async fn save(app: &AppHandle, device: PairedDevice) -> Result<(), String> {
    crate::data::with_store(app, move |store| store.save_paired_device(&device)).await
}
struct NativeDeviceStore<'a>(&'a AppHandle);
impl lifecycle::RemovalStore for NativeDeviceStore<'_> {
    async fn mark_removing(&self, device: &PairedDevice) -> Result<(), String> {
        save(self.0, device.clone()).await
    }
    async fn delete_key(&self, device: &PairedDevice) -> Result<(), String> {
        crate::auth::require_unlocked(self.0).await?;
        crate::credentials::save_pair_key(&device.role, &device.endpoint(), String::new()).await
    }
    async fn delete_record(&self, device: &PairedDevice) -> Result<(), String> {
        let id = device.id.clone();
        crate::data::with_store(self.0, move |store| store.delete_paired_device(&id)).await
    }
}
impl lifecycle::DeviceStore for NativeDeviceStore<'_> {
    async fn ensure_unlocked(&self) -> Result<(), String> {
        crate::auth::require_unlocked(self.0).await
    }
    async fn write_key(&self, device: &PairedDevice, key: &str) -> Result<(), String> {
        crate::credentials::save_pair_key(&device.role, &device.endpoint(), key.to_owned()).await
    }
    async fn write_pending(&self, device: &PairedDevice) -> Result<(), String> {
        save(self.0, device.clone()).await
    }
    async fn commit_active(&self, device: &PairedDevice) -> Result<(), String> {
        crate::data::activate_paired_device(self.0, device.clone()).await
    }
}
impl PairingState {
    pub async fn stop(&self) {
        // Signal running model tunnels before waiting for a slow pairing HTTP operation.
        let tunnels = self
            .1
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .map(|(_, hub)| hub)
            .collect::<Vec<_>>();
        for tunnel in tunnels {
            tunnel.request_stop();
        }
        let mut inner = self.0.lock().await;
        // Closing a pending offer invalidates host consent locally even if the relay is offline.
        if let Some(host) = inner.host.take() {
            let _ = request(
                &host.relay,
                &format!("/{}/host", host.id),
                reqwest::Method::DELETE,
                &host.key,
                None,
                None,
            )
            .await;
        }
        if let Some(client) = inner.client.take() {
            let _ = request(
                &client.relay,
                &format!("/{}/result", client.id),
                reqwest::Method::DELETE,
                &client.key,
                None,
                None,
            )
            .await;
        }
        for (_, tunnel) in inner.tunnels.drain() {
            tunnel.stop().await;
        }
    }
}
async fn reconcile(
    state: &PairingState,
    app: &AppHandle,
    inner: &mut Inner,
    warnings: &mut Vec<String>,
) -> Result<Vec<DeviceStatus>, String> {
    let mut result = Vec::new();
    for mut device in devices(app).await? {
        if device.role == "host" {
            if device.state == "revoking" {
                state
                    .1
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&device.id);
                if let Some(tunnel) = inner.tunnels.remove(&device.id) {
                    tunnel.stop().await;
                }
                let key = crate::credentials::read_pair_key("host", &device.endpoint()).await?;
                if let Some(key) = key {
                    match request(&device.relay_url, &format!("/devices/{}", device.id), reqwest::Method::DELETE, &key, None, relay_token(&device.relay_url).await?).await {
                        Ok(_) => {
                            let mut revoked = device.clone();
                            revoked.state = "revoked".into();
                            match save(app, revoked.clone()).await {
                                Ok(()) => device = revoked,
                                Err(error) => warnings.push(error),
                            }
                        }
                        Err(_) => warnings.push(format!("Removal of {} is pending. Keep Blackwall open and reconnect to the relay.", device.name)),
                    }
                } else {
                    warnings.push(format!("The removal key for {} is missing. Restore its Keychain entry to finish revocation.", device.name));
                }
            }
            if device.state == "active" && !inner.tunnels.contains_key(&device.id) {
                if let Some(key) =
                    crate::credentials::read_pair_key("host", &device.endpoint()).await?
                {
                    let model_key =
                        model_key(app, device.upstream.as_deref().unwrap_or("")).await?;
                    let tunnel = ShareHub::new();
                    tunnel
                        .start_paired(
                            &device,
                            key,
                            model_key,
                            relay_token(&device.relay_url).await?,
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    state
                        .1
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(device.id.clone(), tunnel.clone());
                    if let Err(error) = crate::auth::require_unlocked(app).await {
                        state
                            .1
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&device.id);
                        tunnel.stop().await;
                        return Err(error);
                    }
                    inner.tunnels.insert(device.id.clone(), tunnel);
                } else {
                    warnings.push(format!("The saved host key for {} is missing. Remove this device and pair it again.", device.name));
                }
            }
        }
        let mut display_state = device.state.clone();
        let online = if let Some(tunnel) = inner.tunnels.get(&device.id) {
            let status = tunnel.status().await;
            if !status.active && device.state == "active" {
                display_state = "unavailable".into();
            }
            status.active && !status.network_label.contains("reconnecting")
        } else {
            false
        };
        result.push(DeviceStatus {
            endpoint: device.endpoint(),
            id: device.id,
            role: device.role,
            name: device.name,
            model: device.model,
            state: display_state,
            online,
        });
    }
    Ok(result)
}
#[tauri::command]
pub async fn pairing_status(
    app: AppHandle,
    state: State<'_, PairingState>,
) -> Result<PairingSnapshot, String> {
    let mut inner = state.0.lock().await;
    crate::auth::require_unlocked(&app).await?;
    let mut warnings = Vec::new();
    let mut device_status = reconcile(&state, &app, &mut inner, &mut warnings).await?;
    let host = if let Some(host) = &inner.host {
        if host.expires_at <= now() {
            None
        } else {
            match progress(&host.relay, &host.id, "host", &host.key).await {
                Ok(value) => Some(value),
                Err(error) => {
                    warnings.push(error);
                    None
                }
            }
        }
    } else {
        None
    };
    if inner
        .host
        .as_ref()
        .is_some_and(|host| host.expires_at <= now())
    {
        inner.host = None;
    }
    let client = if let Some(client) = &inner.client {
        if client.expires_at <= now() {
            None
        } else {
            match progress(&client.relay, &client.id, "result", &client.key).await {
                Ok(mut value) => {
                    if value.state == "approved" {
                        let id = value
                            .session_id
                            .clone()
                            .filter(|id| valid_secret(id, "bws_"))
                            .ok_or("The relay returned an invalid paired address.")?;
                        let candidate = value
                            .candidate
                            .clone()
                            .ok_or("Missing approved computer.")?;
                        if !verifies(&candidate.salt, &candidate.hash, &client.key) {
                            return Err(
                                "The approved device credential does not match this computer."
                                    .into(),
                            );
                        }
                        let device = PairedDevice {
                            id,
                            role: "client".into(),
                            name: value.host_name.clone(),
                            relay_url: client.relay.clone(),
                            model: value.model.clone(),
                            upstream: None,
                            salt: candidate.salt,
                            hash: candidate.hash,
                            state: "active".into(),
                        };
                        lifecycle::save_client(&NativeDeviceStore(&app), &device, &client.key)
                            .await?;
                        device_status.retain(|saved| saved.id != device.id);
                        device_status.push(DeviceStatus {
                            endpoint: device.endpoint(),
                            id: device.id,
                            role: device.role,
                            name: device.name,
                            model: device.model,
                            state: device.state,
                            online: false,
                        });
                        value.state = "saved".into();
                    }
                    Some(value)
                }
                Err(error) => {
                    warnings.push(error);
                    None
                }
            }
        }
    } else {
        None
    };
    if inner
        .client
        .as_ref()
        .is_some_and(|client| client.expires_at <= now())
        || client
            .as_ref()
            .is_some_and(|client| matches!(client.state.as_str(), "saved" | "denied"))
    {
        inner.client = None;
    }
    crate::auth::require_unlocked(&app).await?;
    Ok(PairingSnapshot {
        devices: device_status,
        host,
        client,
        warnings,
    })
}
#[tauri::command]
pub async fn create_pairing(
    app: AppHandle,
    state: State<'_, PairingState>,
    relay_url: String,
    relay_key: String,
    name: String,
    model: String,
    endpoint: String,
) -> Result<CreatedInvitation, String> {
    let mut inner = state.0.lock().await;
    crate::auth::require_unlocked(&app).await?;
    validate_name(&name)?;
    validate_model(&model)?;
    let relay = normalize_relay(&relay_url)?;
    let endpoint =
        blackwall_core::connection::normalize_endpoint(&endpoint).map_err(|e| e.to_string())?;
    if devices(&app)
        .await?
        .iter()
        .filter(|d| d.role == "host" && d.state != "revoked")
        .count()
        >= 4
    {
        return Err(
            "Four paired devices are already saved. Remove one before adding another.".into(),
        );
    }
    // Verify the exact chosen model before issuing an invitation.
    let mut probe = http()?.get(format!("{endpoint}/models"));
    if let Some(key) = model_key(&app, &endpoint).await? {
        probe = probe.bearer_auth(key);
    }
    let response = probe
        .send()
        .await
        .map_err(|_| "Open the host's model service and try again.")?;
    if !response.status().is_success() {
        return Err("The host model service rejected access.".into());
    }
    let bytes = read_bounded_response(response, 1024 * 1024)
        .await
        .map_err(|_| "The model list could not be read.")?;
    let catalog: Value =
        serde_json::from_slice(&bytes).map_err(|_| "The model list is invalid.")?;
    if !catalog["data"]
        .as_array()
        .is_some_and(|models| models.iter().any(|item| item["id"] == model))
    {
        return Err("The selected model is no longer available. Choose another model.".into());
    }
    capabilities(&relay).await?;
    let token = if relay_key.trim().is_empty() {
        relay_token(&relay).await?
    } else {
        Some(relay_key.trim().to_owned())
    };
    if let Some(host) = inner.host.take() {
        let _ = request(
            &host.relay,
            &format!("/{}/host", host.id),
            reqwest::Method::DELETE,
            &host.key,
            None,
            None,
        )
        .await;
    }
    let id = secret("bwp_");
    let key = secret("bwh_");
    let invitation_key = secret("bwi_");
    let (salt, hash) = digest(&invitation_key);
    let offer = PairingOffer {
        version: PAIRING_VERSION,
        id: id.clone(),
        host_name: name.clone(),
        model: model.clone(),
        salt,
        hash,
    };
    let value = request(
        &relay,
        "",
        reqwest::Method::POST,
        &key,
        Some(json!(offer)),
        token.clone(),
    )
    .await?;
    let expires_at = value["expiresAt"]
        .as_u64()
        .ok_or("Invalid pairing expiry.")?
        .min(now() + PAIRING_LIFETIME_MS);
    crate::auth::require_unlocked(&app).await?;
    if !relay_key.trim().is_empty() {
        crate::credentials::save_relay_key(&relay, relay_key).await?;
    }
    inner.host = Some(HostOffer {
        relay: relay.clone(),
        id: id.clone(),
        key,
        model,
        upstream: endpoint,
        expires_at,
        device_id: None,
    });
    Ok(CreatedInvitation {
        invitation: format!("{relay}/pair/{id}#key={invitation_key}"),
        expires_at,
    })
}
#[tauri::command]
pub async fn join_pairing(
    app: AppHandle,
    state: State<'_, PairingState>,
    invitation: String,
    name: String,
) -> Result<(), String> {
    let mut inner = state.0.lock().await;
    crate::auth::require_unlocked(&app).await?;
    validate_name(&name)?;
    if devices(&app).await?.len() >= 20 {
        return Err("Remove an older paired computer before adding another.".into());
    }
    let (relay, id, invitation_key) = parse_invitation(&invitation)?;
    capabilities(&relay).await?;
    if inner
        .client
        .as_ref()
        .is_some_and(|client| client.expires_at <= now())
    {
        inner.client = None;
    }
    let was_pending = inner.client.is_some();
    let candidate = if let Some(existing) = &inner.client {
        if existing.relay != relay || existing.id != id {
            return Err("Finish or cancel the current pairing before starting another.".into());
        }
        if existing.candidate.name != name {
            return Err("Cancel the current pairing before changing this computer's name.".into());
        }
        if progress(&relay, &id, "result", &existing.key)
            .await
            .is_ok_and(|value| value.state == "approved")
        {
            return Ok(());
        }
        existing.candidate.clone()
    } else {
        let key = secret("bw1_");
        let (salt, hash) = digest(&key);
        let candidate = PairingCandidate {
            id: secret("bwd_"),
            name,
            salt,
            hash,
        };
        inner.client = Some(ClientRequest {
            relay: relay.clone(),
            id: id.clone(),
            key,
            candidate: candidate.clone(),
            expires_at: now() + PAIRING_LIFETIME_MS,
        });
        candidate
    };
    let result = request(
        &relay,
        &format!("/{id}/join"),
        reqwest::Method::POST,
        &invitation_key,
        Some(json!(candidate)),
        None,
    )
    .await;
    if let Err(error) = result {
        if !was_pending && error.definitive_rejection() {
            inner.client = None;
        }
        return Err(error.into());
    }
    crate::auth::require_unlocked(&app).await
}
#[tauri::command]
pub async fn approve_pairing(
    app: AppHandle,
    state: State<'_, PairingState>,
    candidate: PairingCandidate,
) -> Result<(), String> {
    let mut inner = state.0.lock().await;
    crate::auth::require_unlocked(&app).await?;
    let host = inner
        .host
        .as_mut()
        .ok_or("Create a new pairing invitation.")?;
    let current = progress(&host.relay, &host.id, "host", &host.key).await?;
    if host.expires_at <= now()
        || current.candidate.as_ref() != Some(&candidate)
        || !matches!(current.state.as_str(), "review" | "approved")
    {
        return Err("This pairing request changed or expired. Review a new invitation.".into());
    }
    let id = host.device_id.get_or_insert_with(|| secret("bws_")).clone();
    if let Some(existing) = devices(&app)
        .await?
        .into_iter()
        .find(|device| device.id == id)
    {
        if existing.state == "active" {
            return Ok(());
        }
        if existing.state != "pending" {
            return Err(
                "This device is being removed. Create a new invitation to pair it again.".into(),
            );
        }
    }
    let device = PairedDevice {
        id: id.clone(),
        role: "host".into(),
        name: candidate.name.clone(),
        relay_url: host.relay.clone(),
        model: host.model.clone(),
        upstream: Some(host.upstream.clone()),
        salt: candidate.salt.clone(),
        hash: candidate.hash.clone(),
        state: "pending".into(),
    };
    lifecycle::approve_host(&NativeDeviceStore(&app), &device, &host.key, async {
        request(
            &host.relay,
            &format!("/{}/approve", host.id),
            reqwest::Method::POST,
            &host.key,
            Some(json!(PairingApproval {
                candidate,
                session_id: id
            })),
            None,
        )
        .await
        .map(|_| ())
        .map_err(String::from)
    })
    .await?;
    reconcile(&state, &app, &mut inner, &mut Vec::new()).await?;
    crate::auth::require_unlocked(&app).await
}
#[tauri::command]
pub async fn cancel_pairing(
    app: AppHandle,
    state: State<'_, PairingState>,
    role: String,
) -> Result<(), String> {
    let mut inner = state.0.lock().await;
    crate::auth::require_unlocked(&app).await?;
    if role == "host" {
        if let Some(host) = inner.host.as_ref() {
            if !progress(&host.relay, &host.id, "host", &host.key)
                .await
                .is_ok_and(|value| value.state == "approved")
            {
                let result = request(
                    &host.relay,
                    &format!("/{}/host", host.id),
                    reqwest::Method::DELETE,
                    &host.key,
                    None,
                    None,
                )
                .await;
                if let Err(error) = result {
                    if !error.already_cancelled(false) {
                        return Err(error.into());
                    }
                }
            }
        }
        inner.host = None;
    } else if role == "client" {
        if let Some(client) = inner.client.as_ref() {
            let result = request(
                &client.relay,
                &format!("/{}/result", client.id),
                reqwest::Method::DELETE,
                &client.key,
                None,
                None,
            )
            .await;
            if let Err(error) = result {
                if !error.already_cancelled(true) {
                    return Err(error.into());
                }
            }
        }
        inner.client = None;
    } else {
        return Err("Invalid pairing role.".into());
    }
    Ok(())
}
#[tauri::command]
pub async fn remove_paired_device(
    app: AppHandle,
    state: State<'_, PairingState>,
    id: String,
) -> Result<(), String> {
    let mut inner = state.0.lock().await;
    crate::auth::require_unlocked(&app).await?;
    let mut device = devices(&app)
        .await?
        .into_iter()
        .find(|device| device.id == id)
        .ok_or("This paired computer was not found.")?;
    if device.role == "client" || device.state == "revoked" {
        lifecycle::remove_local(&NativeDeviceStore(&app), &device).await?;
        inner.forget_removed_offer(&id);
    } else {
        device.state = "revoking".into();
        save(&app, device).await?;
        inner.forget_removed_offer(&id);
        if let Some(tunnel) = inner.tunnels.remove(&id) {
            tunnel.stop().await;
        }
        reconcile(&state, &app, &mut inner, &mut Vec::new()).await?;
    }
    Ok(())
}

#[tauri::command]
pub async fn retry_paired_device(
    app: AppHandle,
    state: State<'_, PairingState>,
    id: String,
) -> Result<(), String> {
    let mut inner = state.0.lock().await;
    crate::auth::require_unlocked(&app).await?;
    let device = devices(&app)
        .await?
        .into_iter()
        .find(|device| device.id == id)
        .ok_or("This paired device is missing.")?;
    if device.role != "host" || device.state != "active" {
        return Err(
            "This device cannot reconnect. Pair it again if its access was removed.".into(),
        );
    }
    state
        .1
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    if let Some(tunnel) = inner.tunnels.remove(&id) {
        tunnel.stop().await;
    }
    reconcile(&state, &app, &mut inner, &mut Vec::new()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_invalidates_only_the_matching_approval_offer() {
        let mut inner = Inner {
            host: Some(HostOffer {
                relay: "http://127.0.0.1:1".into(),
                id: secret("bwp_"),
                key: secret("bwh_"),
                model: "test".into(),
                upstream: "http://127.0.0.1:11434/v1".into(),
                expires_at: now() + 60_000,
                device_id: Some("removed-device".into()),
            }),
            ..Inner::default()
        };
        inner.forget_removed_offer("another-device");
        assert!(inner.host.is_some());
        inner.forget_removed_offer("removed-device");
        assert!(inner.host.is_none());
    }

    #[tokio::test]
    async fn lock_stops_tunnels_even_while_an_approval_holds_the_pairing_mutex() {
        let state = std::sync::Arc::new(PairingState::default());
        let (salt, hash) = digest(&secret("bw1_"));
        let device = PairedDevice {
            id: secret("bws_"),
            role: "host".into(),
            name: "Test".into(),
            relay_url: "http://127.0.0.1:1".into(),
            model: "test".into(),
            upstream: Some("http://127.0.0.1:11434/v1".into()),
            salt,
            hash,
            state: "active".into(),
        };
        let hub = ShareHub::new();
        hub.start_paired(&device, secret("bwh_"), None, None)
            .await
            .unwrap();
        state
            .1
            .lock()
            .unwrap()
            .insert(device.id.clone(), hub.clone());
        let mut held = state.0.lock().await;
        held.tunnels.insert(device.id, hub.clone());
        let stopped = state.clone();
        let task = tokio::spawn(async move { stopped.stop().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while hub.status().await.active {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        drop(held);
        task.await.unwrap();
    }
    #[test]
    fn uncertain_responses_keep_credentials_but_rejected_new_requests_can_retry() {
        for status in [None, Some(408), Some(500), Some(502), Some(503)] {
            let error = RelayFailure {
                status,
                message: "Safe error".into(),
            };
            assert!(!error.definitive_rejection());
            assert!(!error.already_cancelled(true));
            assert!(!error.already_cancelled(false));
        }
        for status in [400, 401, 403, 404, 409, 429] {
            let error = RelayFailure {
                status: Some(status),
                message: "Safe error".into(),
            };
            assert!(error.definitive_rejection());
            assert_eq!(
                error.already_cancelled(true),
                status == 401 || status == 404
            );
            assert_eq!(error.already_cancelled(false), status == 404);
        }
    }
}
