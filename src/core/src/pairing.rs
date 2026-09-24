//! Pairing wire types and native-only credential material. Protocol 1 is explicit and bounded.
use crate::share::salted_key_hash;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PAIRING_VERSION: u16 = 1;
pub const PAIRING_LIFETIME_MS: u64 = 5 * 60 * 1000;
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingOffer {
    pub version: u16,
    pub id: String,
    pub host_name: String,
    pub model: String,
    pub salt: String,
    pub hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingCandidate {
    pub id: String,
    pub name: String,
    pub salt: String,
    pub hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingProgress {
    pub state: String,
    pub host_name: String,
    pub model: String,
    pub expires_at: u64,
    pub candidate: Option<PairingCandidate>,
    pub confirmation: Option<String>,
    pub session_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingApproval {
    pub candidate: PairingCandidate,
    pub session_id: String,
}
/// No raw credential fields: safe for SQLite and status IPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairedDevice {
    pub id: String,
    pub role: String,
    pub name: String,
    pub relay_url: String,
    pub model: String,
    /// Host-only upstream. Never transmitted to the relay or receiving device.
    pub upstream: Option<String>,
    pub salt: String,
    pub hash: String,
    pub state: String,
}
impl PairedDevice {
    pub fn endpoint(&self) -> String {
        format!("{}/s/{}/v1", self.relay_url, self.id)
    }
    pub fn validate(&self) -> Result<(), String> {
        if !valid_secret(&self.id, "bws_")
            || !matches!(self.role.as_str(), "host" | "client")
            || !matches!(
                self.state.as_str(),
                "pending" | "active" | "revoking" | "revoked"
            )
        {
            return Err("Invalid paired device record.".into());
        }
        validate_name(&self.name)?;
        validate_model(&self.model)?;
        normalize_relay(&self.relay_url)?;
        if decode_digest(&self.salt).is_none() || decode_digest(&self.hash).is_none() {
            return Err("Invalid device credential digest.".into());
        }
        if self.role == "host" {
            crate::connection::normalize_endpoint(
                self.upstream
                    .as_deref()
                    .ok_or("Missing host model address.")?,
            )
            .map_err(|e| e.to_string())?;
        } else if self.upstream.is_some() {
            return Err("Client records must not contain a host model address.".into());
        }
        Ok(())
    }
}
pub fn secret(prefix: &str) -> String {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}
pub fn valid_secret(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + 43 && value.strip_prefix(prefix).and_then(decode_digest).is_some()
}
pub fn decode_digest(value: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()
}
pub fn digest(key: &str) -> (String, String) {
    let mut salt = [0; 32];
    OsRng.fill_bytes(&mut salt);
    (
        URL_SAFE_NO_PAD.encode(salt),
        URL_SAFE_NO_PAD.encode(salted_key_hash(&salt, key)),
    )
}
pub fn verifies(salt: &str, hash: &str, key: &str) -> bool {
    use subtle::ConstantTimeEq;
    match (decode_digest(salt), decode_digest(hash)) {
        (Some(salt), Some(hash)) => salted_key_hash(&salt, key).ct_eq(&hash).unwrap_u8() == 1,
        _ => false,
    }
}
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
        Err("Use a computer name of 1–120 bytes without control characters.".into())
    } else {
        Ok(())
    }
}
pub fn validate_model(model: &str) -> Result<(), String> {
    if model.trim().is_empty() || model.len() > 512 || model.chars().any(char::is_control) {
        Err("Choose a valid model before pairing.".into())
    } else {
        Ok(())
    }
}
pub fn normalize_relay(value: &str) -> Result<String, String> {
    if value.len() > 2048 {
        return Err("The relay address is too long.".into());
    }
    crate::share::normalize_relay_base_url(value)
        .map_err(|_| "Use an HTTPS relay address without a path or embedded credentials.".into())
}
pub fn confirmation(id: &str, candidate: &PairingCandidate) -> String {
    let bytes = Sha256::new()
        .chain_update(b"blackwall-pairing-confirmation-v1\0")
        .chain_update(id)
        .chain_update(candidate.id.as_bytes())
        .chain_update(candidate.hash.as_bytes())
        .finalize();
    format!(
        "{:02X}{:02X} {:02X}{:02X} {:02X}{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}
pub fn parse_invitation(value: &str) -> Result<(String, String, String), String> {
    if value.len() > 2048 {
        return Err("This pairing invitation is too long.".into());
    }
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| "Paste the complete pairing invitation from the other computer.")?;
    let origin = normalize_relay(&url.origin().ascii_serialization())?;
    let id = url
        .path()
        .strip_prefix("/pair/")
        .ok_or("This is not a computer-pairing invitation.")?;
    let key = url
        .fragment()
        .and_then(|fragment| fragment.strip_prefix("key="))
        .ok_or("This pairing invitation is missing its key.")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || !valid_secret(id, "bwp_")
        || !valid_secret(key, "bwi_")
    {
        return Err(
            "This pairing invitation is invalid. Create a new one on the host computer.".into(),
        );
    }
    Ok((origin, id.to_owned(), key.to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[tokio::test]
    async fn incomplete_revoked_and_client_records_cannot_start_host_tunnels() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-pair-state-{}", rand::random::<u64>()));
        let host_key = secret("bwh_");
        let client_key = secret("bw1_");
        let (salt, hash) = digest(&client_key);
        let mut device = PairedDevice {
            id: secret("bws_"),
            role: "host".into(),
            name: "Test computer".into(),
            relay_url: "http://127.0.0.1:1".into(),
            model: "test-model".into(),
            upstream: Some("http://127.0.0.1:2/v1".into()),
            salt,
            hash,
            state: "pending".into(),
        };
        let hub = crate::share::ShareHub::new();
        for state in ["pending", "revoking", "revoked"] {
            device.state = state.into();
            crate::storage::LocalStore::open(&directory)
                .unwrap()
                .save_paired_device(&device)
                .unwrap();
            let saved = crate::storage::LocalStore::open(&directory)
                .unwrap()
                .paired_devices()
                .unwrap()
                .remove(0);
            assert_eq!(saved.state, state);
            assert!(hub
                .start_paired(&saved, host_key.clone(), None, None)
                .await
                .is_err());
            assert!(!hub.status().await.active);
        }
        device.role = "client".into();
        device.upstream = None;
        device.state = "active".into();
        assert!(hub
            .start_paired(&device, host_key, None, None)
            .await
            .is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn invitation_and_candidate_credentials_are_independent_and_strictly_validated() {
        let id = secret("bwp_");
        let key = secret("bwi_");
        let device_key = secret("bw1_");
        let (salt, hash) = digest(&device_key);
        assert!(verifies(&salt, &hash, &device_key));
        assert!(!verifies(&salt, &hash, &key));
        let url = format!("https://relay.example/pair/{id}#key={key}");
        assert_eq!(parse_invitation(&url).unwrap().0, "https://relay.example");
        assert!(parse_invitation(&url.replace("https:", "http:")).is_err());
        assert!(parse_invitation(&url.replace("relay.example", "secret@relay.example")).is_err());
        assert!(parse_invitation(&format!("{url}&extra=secret")).is_err());
        assert!(parse_invitation(&url.replace("/pair/", "/s/")).is_err());
    }
}
