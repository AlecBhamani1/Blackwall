//! Native lock enforcement and Keychain-protected passphrase verification data.
use serde::Serialize;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;
#[derive(Default)]
struct Inner {
    initialized: bool,
    unlocked: bool,
    locking: bool,
    hash: Option<String>,
    failures: u8,
    retry_after: Option<Instant>,
}
#[derive(Clone, Default)]
pub struct AuthState(std::sync::Arc<Mutex<Inner>>);

pub(crate) struct CommitAuthorization {
    _guard: tokio::sync::OwnedMutexGuard<Inner>,
}
#[derive(Clone, Serialize)]
pub struct AuthStatus {
    pub locked: bool,
    pub enabled: bool,
}
impl Inner {
    fn status(&self) -> AuthStatus {
        AuthStatus {
            locked: !self.unlocked,
            enabled: self.hash.is_some(),
        }
    }
}

impl AuthState {
    pub(crate) async fn ensure_unlocked(&self) -> Result<(), String> {
        if self.0.lock().await.unlocked {
            Ok(())
        } else {
            Err("Unlock Blackwall to continue.".into())
        }
    }
    /// Transfer this guard into the blocking database task, so cancelling the caller
    /// cannot release authorization while its write is still running.
    pub(crate) async fn authorize_commit(&self) -> Result<CommitAuthorization, String> {
        let guard = self.0.clone().lock_owned().await;
        if !guard.unlocked {
            return Err("Unlock Blackwall to continue.".into());
        }
        Ok(CommitAuthorization { _guard: guard })
    }
    pub(crate) async fn begin_lock(&self) -> Result<AuthStatus, String> {
        let mut inner = self.0.lock().await;
        if inner.hash.is_none() {
            return Err("Set a passphrase in Settings before locking Blackwall.".into());
        }
        if inner.locking {
            return Err("Blackwall is still stopping active connections. Please wait.".into());
        }
        inner.unlocked = false;
        inner.locking = true;
        Ok(inner.status())
    }
    async fn lock_with_cleanup(
        &self,
        cleanup: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<AuthStatus, String> {
        let status = self.begin_lock().await?;
        let state = self.clone();
        // IPC cancellation must not strand the app in its intermediate locking state.
        tokio::spawn(async move {
            cleanup.await;
            state.finish_lock().await;
        })
        .await
        .map_err(|_| "Blackwall could not finish stopping its connections.")?;
        Ok(status)
    }
    async fn finish_lock(&self) {
        self.0.lock().await.locking = false;
    }
    async fn unlock(&self, passphrase: String) -> Result<AuthStatus, String> {
        let mut inner = self.0.lock().await;
        if inner.locking {
            return Err("Blackwall is still stopping active connections. Please wait.".into());
        }
        check(&mut inner, passphrase).await?;
        inner.unlocked = true;
        Ok(inner.status())
    }
    #[cfg(test)]
    pub(crate) fn unlocked_for_test() -> Self {
        Self(std::sync::Arc::new(Mutex::new(Inner {
            initialized: true,
            unlocked: true,
            hash: Some("test verifier".into()),
            ..Default::default()
        })))
    }
}

fn read_hash() -> Result<Option<String>, String> {
    let service = crate::identity::keychain_service("com.blackwall.lock");
    #[cfg(target_os = "macos")]
    {
        use security_framework::passwords::{generic_password, PasswordOptions};
        match generic_password(PasswordOptions::new_generic_password(
            &service,
            "passphrase-v1",
        )) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| "The saved app lock is damaged.".into()),
            Err(error) if error.code() == -25300 => Ok(None),
            Err(_) => Err("Unlock your login keychain, then reopen Blackwall.".into()),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = service;
        Ok(None)
    }
}
fn write_hash(hash: Option<&str>) -> Result<(), String> {
    let service = crate::identity::keychain_service("com.blackwall.lock");
    #[cfg(target_os = "macos")]
    {
        use security_framework::passwords::{delete_generic_password, set_generic_password};
        let result = match hash {
            Some(hash) => set_generic_password(&service, "passphrase-v1", hash.as_bytes()),
            None => delete_generic_password(&service, "passphrase-v1"),
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) if hash.is_none() && error.code() == -25300 => Ok(()),
            Err(_) => Err("The app lock could not be saved in Keychain.".into()),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (hash, service);
        Err("The app lock currently requires macOS Keychain.".into())
    }
}
#[tauri::command]
pub async fn auth_status(state: State<'_, AuthState>) -> Result<AuthStatus, String> {
    let mut inner = state.0.lock().await;
    if !inner.initialized {
        inner.hash = crate::keychain::read(read_hash).await?;
        inner.unlocked = inner.hash.is_none();
        inner.initialized = true;
    }
    Ok(inner.status())
}
pub async fn require_unlocked(app: &AppHandle) -> Result<(), String> {
    app.state::<AuthState>().ensure_unlocked().await
}

async fn check(inner: &mut Inner, passphrase: String) -> Result<(), String> {
    if inner
        .retry_after
        .is_some_and(|deadline| deadline > Instant::now())
    {
        return Err("Too many attempts. Wait a few seconds and try again.".into());
    }
    let hash = inner
        .hash
        .clone()
        .ok_or("No app passphrase is configured.")?;
    if !tokio::task::spawn_blocking(move || blackwall_core::auth::verify(&passphrase, &hash))
        .await
        .map_err(|_| "Passphrase verification failed.")?
    {
        inner.failures = inner.failures.saturating_add(1);
        let seconds = if inner.failures >= 5 { 30 } else { 2 };
        inner.retry_after = Some(Instant::now() + Duration::from_secs(seconds));
        return Err("That passphrase did not match. Try again.".into());
    }
    inner.failures = 0;
    inner.retry_after = None;
    Ok(())
}
#[tauri::command]
pub async fn unlock_app(
    passphrase: String,
    state: State<'_, AuthState>,
) -> Result<AuthStatus, String> {
    state.unlock(passphrase).await
}

#[tauri::command]
pub async fn set_passphrase(
    current: String,
    passphrase: String,
    state: State<'_, AuthState>,
) -> Result<AuthStatus, String> {
    let mut inner = state.0.lock().await;
    if !inner.unlocked {
        return Err("Unlock Blackwall first.".into());
    }
    if inner.hash.is_some() {
        check(&mut inner, current).await?;
    }
    let hash = tokio::task::spawn_blocking(move || {
        let hash = if passphrase.is_empty() {
            None
        } else {
            Some(blackwall_core::auth::hash(&passphrase).map_err(|e| e.to_string())?)
        };
        write_hash(hash.as_deref())?;
        Ok::<_, String>(hash)
    })
    .await
    .map_err(|_| "The app lock could not be saved.")??;
    inner.hash = hash;
    Ok(inner.status())
}
#[tauri::command]
pub async fn lock_app(app: AppHandle, state: State<'_, AuthState>) -> Result<AuthStatus, String> {
    state
        .lock_with_cleanup(async move {
            app.state::<crate::setup::SetupState>().jobs.cancel_all();
            let pairing = app.state::<crate::pairing::PairingState>();
            let commands = app.state::<crate::commands::AppState>();
            tokio::join!(pairing.stop(), commands.share_hub.stop());
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_lock_caller_does_not_cancel_connection_cleanup() {
        let state = AuthState::unlocked_for_test();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let resume = std::sync::Arc::new(tokio::sync::Notify::new());
        let (signal, wait, locking) = (entered.clone(), resume.clone(), state.clone());
        let caller = tokio::spawn(async move {
            locking
                .lock_with_cleanup(async move {
                    signal.notify_one();
                    wait.notified().await;
                })
                .await
        });
        entered.notified().await;
        caller.abort();
        assert!(caller.await.is_err());
        assert!(state
            .unlock("unused".into())
            .await
            .is_err_and(|error| error.contains("stopping")));
        resume.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.0.lock().await.locking {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(state.ensure_unlocked().await.is_err());
    }
    #[tokio::test]
    async fn unlock_and_duplicate_lock_are_rejected_until_connection_cleanup_finishes() {
        let state = AuthState::unlocked_for_test();
        assert!(state.begin_lock().await.is_ok());
        assert!(state
            .unlock("unused".into())
            .await
            .is_err_and(|error| error.contains("stopping")));
        assert!(state.begin_lock().await.is_err());
        assert!(state.ensure_unlocked().await.is_err());
        state.finish_lock().await;
        // Cleanup completion does not itself unlock the app.
        assert!(state.ensure_unlocked().await.is_err());
        assert!(!state.0.lock().await.locking);
    }
}
