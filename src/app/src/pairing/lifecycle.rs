//! Ordering shared by native commands and fault-injection tests. Only a committed
//! active record may restore a tunnel; uncertain approval always retains its identity.
use blackwall_core::pairing::PairedDevice;
use std::future::Future;

pub(super) trait DeviceStore {
    fn ensure_unlocked(&self) -> impl Future<Output = Result<(), String>> + Send;
    fn write_key(
        &self,
        device: &PairedDevice,
        key: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn write_pending(
        &self,
        device: &PairedDevice,
    ) -> impl Future<Output = Result<(), String>> + Send;
    /// Serialize this durable commit with the transition into the locked state.
    fn commit_active(
        &self,
        device: &PairedDevice,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

pub(super) async fn approve_host(
    store: &impl DeviceStore,
    device: &PairedDevice,
    key: &str,
    publish: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    device.validate()?;
    if device.role != "host" || device.state != "pending" {
        return Err("This device cannot be approved.".into());
    }
    store.ensure_unlocked().await?;
    store.write_key(device, key).await?;
    store.ensure_unlocked().await?;
    store.write_pending(device).await?;
    store.ensure_unlocked().await?;
    publish.await?;
    store.commit_active(device).await
}

pub(super) async fn save_client(
    store: &impl DeviceStore,
    device: &PairedDevice,
    key: &str,
) -> Result<(), String> {
    device.validate()?;
    if device.role != "client" || device.state != "active" {
        return Err("This device cannot be saved as a client.".into());
    }
    store.ensure_unlocked().await?;
    store.write_key(device, key).await?;
    store.commit_active(device).await
}

pub(super) trait RemovalStore {
    fn mark_removing(
        &self,
        device: &PairedDevice,
    ) -> impl Future<Output = Result<(), String>> + Send;
    fn delete_key(&self, device: &PairedDevice) -> impl Future<Output = Result<(), String>> + Send;
    fn delete_record(
        &self,
        device: &PairedDevice,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

/// Persist intent before changing Keychain. A failed cleanup must never leave an
/// active-looking record whose credential has already been deleted.
pub(super) async fn remove_local(
    store: &impl RemovalStore,
    device: &PairedDevice,
) -> Result<(), String> {
    device.validate()?;
    if device.role != "client" && device.state != "revoked" {
        return Err("Revoke this computer before removing its local credentials.".into());
    }
    let mut removing = device.clone();
    if removing.role == "client" && removing.state != "revoked" {
        removing.state = "revoking".into();
    }
    store.mark_removing(&removing).await?;
    store.delete_key(&removing).await?;
    store.delete_record(&removing).await
}

#[cfg(test)]
mod tests;
