//! Native lock enforcement and passphrase verification data kept in the credential store.
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;
const LOCK_ACCOUNT: &str = "passphrase-v1";
/// Empty, non-secret file in the data folder: an app lock exists, even when the keyring cannot say so.
pub(crate) const LOCK_MARKER: &str = "app-lock-enabled";
pub(crate) const KEYRING_NEEDED: &str = "Your app lock is stored in the system keyring, which isn't available. Start your keyring (GNOME Keyring or KWallet), then try again.";
#[derive(Default)]
struct Inner {
    initialized: bool,
    unlocked: bool,
    locking: bool,
    hash: Option<String>,
    keyring: bool,
    keyring_needed: bool,
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
    pub keyring: bool,
    #[serde(rename = "keyringNeeded")]
    pub keyring_needed: bool,
}
impl Inner {
    fn status(&self) -> AuthStatus {
        AuthStatus {
            locked: !self.unlocked,
            // A stored lock the keyring cannot show us is still a lock.
            enabled: self.hash.is_some() || self.keyring_needed,
            keyring: self.keyring,
            keyring_needed: self.keyring_needed,
        }
    }
}
enum LockRead {
    Found(String),
    Missing,
    Unavailable,
    Failed(String),
}
#[derive(Debug, PartialEq)]
enum Startup {
    Open { keyring: bool },
    Locked(String),
    KeyringNeeded,
    Retry(String),
}
/// Only a fresh data folder (no marker, no conversations) may open without a keyring.
fn startup(read: LockRead, marker: bool, has_sessions: bool) -> Startup {
    match read {
        LockRead::Found(hash) => Startup::Locked(hash),
        LockRead::Missing => Startup::Open { keyring: true },
        LockRead::Unavailable if marker || has_sessions => Startup::KeyringNeeded,
        LockRead::Unavailable => Startup::Open { keyring: false },
        LockRead::Failed(message) => Startup::Retry(message),
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
        if inner.keyring_needed {
            return Err(KEYRING_NEEDED.into());
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
            keyring: true,
            hash: Some("test verifier".into()),
            ..Default::default()
        })))
    }
}

fn read_hash() -> LockRead {
    let service = crate::identity::keychain_service("com.blackwall.lock");
    match crate::secrets::get(&service, LOCK_ACCOUNT) {
        Ok(Some(hash)) => LockRead::Found(hash),
        Ok(None) => LockRead::Missing,
        Err(crate::secrets::SecretError::Unavailable) => LockRead::Unavailable,
        Err(crate::secrets::SecretError::Invalid) => {
            LockRead::Failed("The saved app lock is damaged.".into())
        }
        Err(error) => LockRead::Failed(error.explain(format!(
            "Blackwall could not read its app lock from {}. {}, then reopen Blackwall.",
            crate::secrets::STORE,
            crate::secrets::RECOVER
        ))),
    }
}
fn write_hash(hash: Option<&str>) -> Result<(), String> {
    let service = crate::identity::keychain_service("com.blackwall.lock");
    match hash {
        Some(hash) => crate::secrets::set(&service, LOCK_ACCOUNT, hash),
        None => crate::secrets::delete(&service, LOCK_ACCOUNT),
    }
    .map_err(|error| {
        error.explain(format!(
            "The app lock could not be saved in {}.",
            crate::secrets::STORE
        ))
    })
}
fn write_marker(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    std::fs::write(directory.join(LOCK_MARKER), b"")
}
fn remove_marker(directory: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(directory.join(LOCK_MARKER)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
/// Keep the marker in step with the stored lock. The marker changes first, so a failure
/// to remove it leaves the lock in place rather than an unlocked app that still looks locked.
fn change_lock(
    directory: &Path,
    hash: Option<&str>,
    store: impl FnOnce(Option<&str>) -> Result<(), String>,
) -> Result<(), String> {
    let had_marker = directory.join(LOCK_MARKER).exists();
    match hash {
        Some(_) => write_marker(directory),
        None => remove_marker(directory),
    }
    .map_err(|_| "The app lock marker could not be updated in your data folder.".to_string())?;
    let stored = store(hash);
    if stored.is_err() && hash.is_some() && !had_marker {
        // A lock that was never saved must not leave a marker that locks the next launch.
        let _ = remove_marker(directory);
    }
    stored
}
fn has_sessions(directory: &Path) -> bool {
    // If the data cannot be inspected, assume conversations exist and stay locked.
    blackwall_core::storage::LocalStore::open(directory)
        .and_then(|store| store.has_sessions())
        .unwrap_or(true)
}
#[tauri::command]
pub async fn auth_status(
    app: AppHandle,
    state: State<'_, AuthState>,
) -> Result<AuthStatus, String> {
    let mut inner = state.0.lock().await;
    if !inner.initialized {
        let directory = crate::identity::data_directory(&app)?;
        let read = crate::keychain::read(|| Ok(read_hash())).await?;
        let dir = directory.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let marker = dir.join(LOCK_MARKER).exists();
            // Best effort: a stale marker only matters once the keyring is gone.
            match &read {
                LockRead::Found(_) => drop(write_marker(&dir)),
                LockRead::Missing => drop(remove_marker(&dir)),
                _ => {}
            }
            let sessions = matches!(read, LockRead::Unavailable) && !marker && has_sessions(&dir);
            startup(read, marker, sessions)
        })
        .await
        .map_err(|_| "Blackwall could not read its app lock.")?;
        // Only Locked and Open are settled; the other outcomes read the keyring again next time.
        match outcome {
            Startup::Retry(message) => return Err(message),
            Startup::KeyringNeeded => {
                inner.keyring_needed = true;
                inner.keyring = false;
                inner.unlocked = false;
                inner.hash = None;
                return Ok(inner.status());
            }
            Startup::Locked(hash) => {
                inner.hash = Some(hash);
                inner.unlocked = false;
                inner.keyring = true;
            }
            Startup::Open { keyring } => {
                inner.hash = None;
                inner.unlocked = true;
                inner.keyring = keyring;
            }
        }
        inner.keyring_needed = false;
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
    app: AppHandle,
    current: String,
    passphrase: String,
    state: State<'_, AuthState>,
) -> Result<AuthStatus, String> {
    let mut inner = state.0.lock().await;
    if !inner.unlocked {
        return Err("Unlock Blackwall first.".into());
    }
    if !inner.keyring {
        return Err(crate::secrets::SecretError::Unavailable.explain(""));
    }
    if inner.hash.is_some() {
        check(&mut inner, current).await?;
    }
    let directory = crate::identity::data_directory(&app)?;
    let hash = tokio::task::spawn_blocking(move || {
        let hash = if passphrase.is_empty() {
            None
        } else {
            Some(blackwall_core::auth::hash(&passphrase).map_err(|e| e.to_string())?)
        };
        change_lock(&directory, hash.as_deref(), write_hash)?;
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
    #[test]
    fn startup_opens_only_fresh_data_without_keyring() {
        assert_eq!(
            startup(LockRead::Unavailable, false, false),
            Startup::Open { keyring: false }
        );
        assert_eq!(
            startup(LockRead::Unavailable, true, false),
            Startup::KeyringNeeded
        );
    }
    #[test]
    fn startup_dismissed_prompt_retries_and_existing_data_without_marker_stays_locked() {
        assert!(matches!(
            startup(LockRead::Failed("dismissed".into()), false, false),
            Startup::Retry(_)
        ));
        assert_eq!(
            startup(LockRead::Unavailable, false, true),
            Startup::KeyringNeeded
        );
    }
    #[test]
    fn found_lock_is_locked_and_missing_lock_opens() {
        assert_eq!(
            startup(LockRead::Found("h".into()), false, true),
            Startup::Locked("h".into())
        );
        assert_eq!(
            startup(LockRead::Missing, true, true),
            Startup::Open { keyring: true }
        );
    }
    fn data_folder(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-auth-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }
    #[test]
    fn failed_lock_save_leaves_no_new_marker_but_keeps_an_existing_one() {
        let directory = data_folder("failed-save");
        let marker = directory.join(LOCK_MARKER);
        let failing = |_: Option<&str>| Err("no keyring".to_string());
        assert!(change_lock(&directory, Some("hash"), failing).is_err());
        assert!(
            !marker.exists(),
            "a lock that was never saved must not leave a marker"
        );
        write_marker(&directory).unwrap();
        assert!(change_lock(&directory, Some("hash"), failing).is_err());
        assert!(marker.exists(), "an existing lock keeps its marker");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn marker_tracks_lock_and_failed_removal_keeps_lock() {
        use std::cell::Cell;
        let directory = data_folder("marker");
        let marker = directory.join(LOCK_MARKER);
        let stored = Cell::new(0);
        let store = |_: Option<&str>| {
            stored.set(stored.get() + 1);
            Ok(())
        };
        change_lock(&directory, Some("hash"), store).unwrap();
        assert!(marker.exists());
        assert_eq!(stored.get(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
            // A directory that is still writable here (running as root) cannot prove failure.
            if std::fs::File::create(directory.join("probe")).is_err() {
                assert!(change_lock(&directory, None, store).is_err());
                assert!(marker.exists());
                assert_eq!(stored.get(), 1, "the lock must stay when the marker stays");
            }
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        change_lock(&directory, None, store).unwrap();
        assert!(!marker.exists());
        assert_eq!(stored.get(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
