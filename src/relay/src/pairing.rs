//! Ephemeral five-minute exchanges; approved device ownership/revocation is durable.
use super::*;
use axum::routing::delete;
use blackwall_core::pairing::*;
use std::collections::VecDeque;

#[derive(Default)]
pub(crate) struct Exchanges {
    entries: HashMap<String, Exchange>,
    /// A deployment-wide rate cap also works behind a TLS proxy without trusting spoofable IP headers.
    /// Only invitation creation, revocation and failed lookups/credentials are charged, so junk
    /// traffic cannot lock authenticated hosts and devices out of their own exchanges.
    requests: VecDeque<u64>,
}
struct Exchange {
    offer: PairingOffer,
    host_digest: [u8; 32],
    expires_at: u64,
    candidate: Option<PairingCandidate>,
    failures: u8,
    state: String,
    session_id: Option<String>,
}
pub(crate) fn routes() -> Router<RelayState> {
    Router::new()
        .route(
            "/v1/pairing/capabilities",
            get(|| async { no_store(Json(json!({"version": PAIRING_VERSION})).into_response()) }),
        )
        .route("/v1/pairing", post(create))
        .route("/v1/pairing/{id}/host", get(host_status).delete(cancel))
        .route("/v1/pairing/{id}/join", post(join))
        .route(
            "/v1/pairing/{id}/result",
            get(result).delete(cancel_candidate),
        )
        .route("/v1/pairing/{id}/approve", post(approve))
        .route("/v1/pairing/devices/{id}", delete(revoke))
        .layer(axum::extract::DefaultBodyLimit::max(4096))
}
fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
}
fn error(message: &str) -> RelayApiError {
    RelayApiError::bad_request(message)
}
fn host_digest(id: &str, key: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::new()
        .chain_update(id)
        .chain_update(key)
        .finalize()
        .into()
}
fn authorize_host(exchange: &Exchange, key: &str) -> Result<(), RelayApiError> {
    if exchange
        .host_digest
        .ct_eq(&host_digest(&exchange.offer.id, key))
        .unwrap_u8()
        != 1
    {
        return Err(RelayApiError::unauthorized(
            "Pairing host credential is invalid.",
        ));
    }
    Ok(())
}
impl Exchanges {
    fn limit(&mut self) -> Result<(), RelayApiError> {
        let now = unix_epoch_millis();
        self.entries.retain(|_, entry| entry.expires_at > now);
        while self
            .requests
            .front()
            .is_some_and(|time| now.saturating_sub(*time) > 60_000)
        {
            self.requests.pop_front();
        }
        if self.requests.len() >= 1200 {
            return Err(RelayApiError::too_many_requests(
                "Pairing is busy. Try again in a minute.",
            ));
        }
        self.requests.push_back(now);
        Ok(())
    }
    /// Charges a rejected request; past the budget it is reported as busy instead.
    fn refuse(&mut self, error: RelayApiError) -> RelayApiError {
        self.limit().err().unwrap_or(error)
    }
    fn get(&mut self, id: &str) -> Result<&mut Exchange, RelayApiError> {
        let now = unix_epoch_millis();
        self.entries.retain(|_, entry| entry.expires_at > now);
        if !self.entries.contains_key(id) {
            return Err(self.refuse(RelayApiError::not_found(
                "This pairing invitation expired or was cancelled. Create a new invitation.",
            )));
        }
        self.entries
            .get_mut(id)
            .ok_or_else(|| RelayApiError::not_found("This pairing invitation expired."))
    }
    fn host(&mut self, id: &str, key: &str) -> Result<&mut Exchange, RelayApiError> {
        if let Err(error) = authorize_host(self.get(id)?, key) {
            return Err(self.refuse(error));
        }
        self.get(id)
    }
    fn candidate(&mut self, id: &str, key: &str) -> Result<&mut Exchange, RelayApiError> {
        let candidate = self.get(id)?.candidate.as_ref();
        if !candidate.is_some_and(|candidate| verifies(&candidate.salt, &candidate.hash, key)) {
            return Err(self.refuse(RelayApiError::unauthorized("Device credential is invalid.")));
        }
        self.get(id)
    }
}
impl Exchange {
    fn progress(&self) -> PairingProgress {
        PairingProgress {
            state: self.state.clone(),
            host_name: self.offer.host_name.clone(),
            model: self.offer.model.clone(),
            expires_at: self.expires_at,
            candidate: self.candidate.clone(),
            confirmation: self
                .candidate
                .as_ref()
                .map(|candidate| confirmation(&self.offer.id, candidate)),
            session_id: self.session_id.clone(),
        }
    }
}
async fn create(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(offer): Json<PairingOffer>,
) -> Result<Response, RelayApiError> {
    if let Some(expected) = &state.config.registration_token {
        let token = headers
            .get("x-blackwall-relay-token")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !constant_time_text_eq(expected, token) {
            return Err(RelayApiError::unauthorized(
                "The relay registration token is invalid.",
            ));
        }
    }
    if offer.version != PAIRING_VERSION
        || !valid_secret(&offer.id, "bwp_")
        || !valid_secret(bearer(&headers), "bwh_")
        || decode_digest(&offer.salt).is_none()
        || decode_digest(&offer.hash).is_none()
    {
        return Err(error("Invalid pairing invitation."));
    }
    validate_name(&offer.host_name).map_err(|e| error(&e))?;
    validate_model(&offer.model).map_err(|e| error(&e))?;
    let mut exchanges = state.pairing.lock().await;
    exchanges.limit()?;
    if exchanges.entries.contains_key(&offer.id) {
        return Err(error(
            "This pairing invitation already exists. Create a new invitation.",
        ));
    }
    if exchanges.entries.len() >= 256 {
        return Err(RelayApiError::too_many_requests(
            "Too many pending pairing invitations.",
        ));
    }
    let exchange = Exchange {
        host_digest: host_digest(&offer.id, bearer(&headers)),
        offer,
        expires_at: unix_epoch_millis() + PAIRING_LIFETIME_MS,
        candidate: None,
        failures: 0,
        state: "waiting".into(),
        session_id: None,
    };
    let progress = exchange.progress();
    exchanges
        .entries
        .insert(exchange.offer.id.clone(), exchange);
    Ok(no_store(Json(progress).into_response()))
}
async fn host_status(
    State(state): State<RelayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayApiError> {
    let mut exchanges = state.pairing.lock().await;
    let exchange = exchanges.host(&id, bearer(&headers))?;
    Ok(no_store(Json(exchange.progress()).into_response()))
}
async fn cancel(
    State(state): State<RelayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayApiError> {
    let mut exchanges = state.pairing.lock().await;
    let exchange = exchanges.host(&id, bearer(&headers))?;
    if exchange.state == "approved" {
        return Err(error("Remove the approved device to revoke its access."));
    }
    exchange.state = "denied".into();
    Ok(no_store(Json(exchange.progress()).into_response()))
}
async fn join(
    State(state): State<RelayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(candidate): Json<PairingCandidate>,
) -> Result<Response, RelayApiError> {
    validate_name(&candidate.name).map_err(|e| error(&e))?;
    if !valid_secret(&candidate.id, "bwd_")
        || decode_digest(&candidate.salt).is_none()
        || decode_digest(&candidate.hash).is_none()
    {
        return Err(error("Invalid pairing candidate."));
    }
    let mut exchanges = state.pairing.lock().await;
    let exchange = exchanges.get(&id)?;
    if exchange.failures >= 5 {
        return Err(RelayApiError::too_many_requests(
            "This invitation is locked. Create a new invitation.",
        ));
    }
    if !verifies(&exchange.offer.salt, &exchange.offer.hash, bearer(&headers)) {
        exchange.failures += 1;
        return Err(exchanges.refuse(RelayApiError::unauthorized(
            "The pairing invitation key is invalid.",
        )));
    }
    if !matches!(exchange.state.as_str(), "waiting" | "review") {
        return Err(error("This invitation is no longer accepting a computer."));
    }
    if exchange
        .candidate
        .as_ref()
        .is_some_and(|existing| existing != &candidate)
    {
        return Err(error(
            "Another computer has already requested this invitation. Create a new one.",
        ));
    }
    exchange.candidate = Some(candidate);
    exchange.state = "review".into();
    Ok(no_store(Json(exchange.progress()).into_response()))
}
async fn result(
    State(state): State<RelayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayApiError> {
    let mut exchanges = state.pairing.lock().await;
    let exchange = exchanges.candidate(&id, bearer(&headers))?;
    Ok(no_store(Json(exchange.progress()).into_response()))
}
async fn cancel_candidate(
    State(state): State<RelayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayApiError> {
    let mut exchanges = state.pairing.lock().await;
    let exchange = exchanges.candidate(&id, bearer(&headers))?;
    if exchange.state == "approved" {
        return Err(error(
            "This device was already approved. Remove its saved connection.",
        ));
    }
    exchange.state = "denied".into();
    Ok(no_store(Json(exchange.progress()).into_response()))
}
async fn approve(
    State(state): State<RelayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(approval): Json<PairingApproval>,
) -> Result<Response, RelayApiError> {
    if !valid_secret(&approval.session_id, "bws_") {
        return Err(error("Invalid paired address."));
    }
    let mut exchanges = state.pairing.lock().await;
    let exchange = exchanges.host(&id, bearer(&headers))?;
    if exchange.candidate.as_ref() != Some(&approval.candidate)
        || !matches!(exchange.state.as_str(), "review" | "approved")
    {
        return Err(error(
            "This pairing request changed or was cancelled. Review a new request.",
        ));
    }
    if exchange
        .session_id
        .as_ref()
        .is_some_and(|id| id != &approval.session_id)
    {
        return Err(error("This invitation has already paired a computer."));
    }
    // No live session uses this address yet, so the claim needs no session lock.
    let ownership = state.ownership.clone();
    let session_id = approval.session_id.clone();
    let key = bearer(&headers).to_owned();
    tokio::task::spawn_blocking(move || ownership.claim(&session_id, &key))
        .await
        .map_err(|_| error("Pairing storage is unavailable."))?
        .map_err(|_| error("The paired address could not be reserved."))?;
    exchange.state = "approved".into();
    exchange.session_id = Some(approval.session_id);
    Ok(no_store(Json(exchange.progress()).into_response()))
}
async fn revoke(
    State(state): State<RelayState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayApiError> {
    if let Some(expected) = &state.config.registration_token {
        let token = headers
            .get("x-blackwall-relay-token")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !constant_time_text_eq(expected, token) {
            return Err(RelayApiError::unauthorized(
                "The relay registration token is invalid.",
            ));
        }
    }
    if !valid_secret(&id, "bws_") || !valid_secret(bearer(&headers), "bwh_") {
        return Err(error("Invalid device revocation."));
    }
    let ownership = state.ownership.clone();
    let session_id = id.clone();
    let key = bearer(&headers).to_owned();
    // The owner of a known address is never charged, so junk traffic cannot block revocation.
    let revoked = tokio::task::spawn_blocking(move || ownership.revoke_existing(&session_id, &key))
        .await
        .map_err(|_| error("Revocation storage is unavailable."))?;
    let denied = || RelayApiError::unauthorized("Device revocation could not be authorized.");
    match revoked {
        Ok(true) => {}
        Ok(false) => {
            state.pairing.lock().await.limit()?;
            let ownership = state.ownership.clone();
            let session_id = id.clone();
            let key = bearer(&headers).to_owned();
            // Reserve-and-revoke also handles removal before the first tunnel registration.
            tokio::task::spawn_blocking(move || ownership.revoke(&session_id, &key))
                .await
                .map_err(|_| error("Revocation storage is unavailable."))?
                .map_err(|_| denied())?;
        }
        Err(_) => return Err(state.pairing.lock().await.refuse(denied())),
    }
    // Bump only after the durable write so a racing registration retries its claim.
    state.revocations.fetch_add(1, Ordering::SeqCst);
    let previous = state.sessions.write().await.remove(&id);
    if let Some(previous) = previous {
        previous
            .disconnect("This paired computer was removed by its owner.")
            .await;
        let _ = previous
            .send_to_host(&RelayToHost::Error {
                message: "This paired device has been revoked.".into(),
            })
            .await;
    }
    Ok(no_store(Json(json!({"revoked":true})).into_response()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn expired_exchanges_are_removed_and_request_rate_is_bounded() {
        let mut exchanges = Exchanges::default();
        let id = secret("bwp_");
        let (salt, hash) = digest(&secret("bwi_"));
        exchanges.entries.insert(
            id.clone(),
            Exchange {
                offer: PairingOffer {
                    version: 1,
                    id: id.clone(),
                    host_name: "Home".into(),
                    model: "model".into(),
                    salt,
                    hash,
                },
                host_digest: [0; 32],
                expires_at: unix_epoch_millis() - 1,
                candidate: None,
                failures: 0,
                state: "waiting".into(),
                session_id: None,
            },
        );
        assert!(exchanges.get(&id).is_err());
        assert!(exchanges.entries.is_empty());
        for _ in 0..1199 {
            exchanges.limit().unwrap();
        }
        assert!(exchanges.limit().is_err());
        exchanges.requests.clear();
        exchanges.limit().unwrap();
    }
    #[test]
    fn junk_requests_cannot_exhaust_the_budget_for_authenticated_parties() {
        let mut exchanges = Exchanges::default();
        let id = secret("bwp_");
        let host = secret("bwh_");
        let (salt, hash) = digest(&secret("bwi_"));
        let device = secret("bw1_");
        let (device_salt, device_hash) = digest(&device);
        exchanges.entries.insert(
            id.clone(),
            Exchange {
                offer: PairingOffer {
                    version: 1,
                    id: id.clone(),
                    host_name: "Home".into(),
                    model: "model".into(),
                    salt,
                    hash,
                },
                host_digest: host_digest(&id, &host),
                expires_at: unix_epoch_millis() + 60_000,
                candidate: Some(PairingCandidate {
                    id: secret("bwd_"),
                    name: "Laptop".into(),
                    salt: device_salt,
                    hash: device_hash,
                }),
                failures: 0,
                state: "review".into(),
                session_id: None,
            },
        );
        for _ in 0..1200 {
            assert!(exchanges.get(&secret("bwp_")).is_err());
        }
        let busy = |error: RelayApiError| error.status == StatusCode::TOO_MANY_REQUESTS;
        assert!(exchanges.get(&secret("bwp_")).is_err_and(busy));
        assert!(exchanges.host(&id, "bwh_wrong").is_err_and(busy));
        assert!(exchanges.candidate(&id, "bw1_wrong").is_err_and(busy));
        for _ in 0..10 {
            exchanges.host(&id, &host).unwrap();
            exchanges.candidate(&id, &device).unwrap();
        }
    }
    #[tokio::test]
    async fn junk_lookups_cannot_block_an_owner_from_revoking_a_device() {
        let state = RelayState {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config: RelayConfig::default(),
            ownership: Arc::new(OwnershipStore::memory()),
            pairing: Arc::new(tokio::sync::Mutex::new(Exchanges::default())),
            revocations: Arc::new(AtomicU64::new(0)),
            reserved: Arc::new(AtomicUsize::new(0)),
        };
        let device = secret("bws_");
        let owner = secret("bwh_");
        state.ownership.claim(&device, &owner).unwrap();
        for _ in 0..1200 {
            assert!(state.pairing.lock().await.get(&secret("bwp_")).is_err());
        }
        let headers = |key: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, format!("Bearer {key}").parse().unwrap());
            headers
        };
        let busy = |error: RelayApiError| error.status == StatusCode::TOO_MANY_REQUESTS;
        let attempt =
            |id: String, key: String| revoke(State(state.clone()), Path(id), headers(&key));
        assert!(attempt(device.clone(), secret("bwh_"))
            .await
            .is_err_and(busy));
        assert!(attempt(secret("bws_"), secret("bwh_"))
            .await
            .is_err_and(busy));
        attempt(device.clone(), owner.clone()).await.unwrap();
        assert!(state.ownership.claim(&device, &owner).is_err());
    }
}
