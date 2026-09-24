//! A bounded set of independently revocable guest invitations for one desktop.
use super::{ShareError, ShareHub, ShareStatus, StartShareRequest};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const MAX_INVITES: usize = 4;
struct Entry {
    id: String,
    name: String,
    hub: ShareHub,
}
/// Public metadata. The credential-bearing fields are populated only during creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedShare {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub status: ShareStatus,
}
/// Owns up to four invites; with two requests each, guest concurrency is bounded to eight.
#[derive(Default)]
pub struct ShareManager {
    entries: Mutex<Vec<Entry>>,
}
impl ShareManager {
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn start_named(
        &self,
        request: StartShareRequest,
        model_key: Option<String>,
        name: String,
    ) -> Result<ManagedShare, ShareError> {
        let name = name.trim();
        if name.len() > 120 || name.chars().any(char::is_control) {
            return Err(ShareError::InvalidInviteName);
        }
        // Serialize creation and revocation, so locking during registration cannot orphan a share.
        let mut entries = self.entries.lock().await;
        let mut index = 0;
        while index < entries.len() {
            if entries[index].hub.status().await.active {
                index += 1;
            } else {
                entries.remove(index).hub.stop().await;
            }
        }
        if entries.len() >= MAX_INVITES {
            return Err(ShareError::InviteLimit);
        }
        let hub = ShareHub::new();
        let status = hub.start_with_model_key(request, model_key).await?;
        let id = format!("invite_{:016x}", rand::random::<u64>());
        let name = if name.is_empty() {
            let mut number = 1;
            while entries
                .iter()
                .any(|entry| entry.name == format!("Guest link {number}"))
            {
                number += 1;
            }
            format!("Guest link {number}")
        } else {
            name.to_owned()
        };
        entries.push(Entry {
            id: id.clone(),
            name: name.clone(),
            hub,
        });
        Ok(ManagedShare { id, name, status })
    }
    pub async fn list(&self) -> Vec<ManagedShare> {
        let entries = self.entries.lock().await;
        let mut result = Vec::new();
        for entry in entries.iter() {
            let status = entry.hub.status().await;
            if status.active {
                result.push(ManagedShare {
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                    status,
                });
            }
        }
        result
    }
    /// Latest live invite, for compatibility with the single-link settings view.
    pub async fn status(&self) -> ManagedShare {
        self.list().await.pop().unwrap_or(ManagedShare {
            id: String::new(),
            name: String::new(),
            status: ShareStatus::inactive(),
        })
    }
    pub async fn revoke(&self, id: &str) -> bool {
        let mut entries = self.entries.lock().await;
        if let Some(index) = entries.iter().position(|entry| entry.id == id) {
            entries.remove(index).hub.stop().await;
            true
        } else {
            false
        }
    }
    /// Revokes every invite, including an in-progress serialized registration.
    pub async fn stop(&self) -> ShareStatus {
        let mut entries = self.entries.lock().await;
        for entry in entries.drain(..) {
            entry.hub.stop().await;
        }
        ShareStatus::inactive()
    }
}
