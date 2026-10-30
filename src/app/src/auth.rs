//! Native lock enforcement and passphrase verification data kept in the credential store.
use blackwall_core::storage::{DATABASE_FILE, LOCK_MARKER, NO_LOCK_MARKER};
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;
const LOCK_ACCOUNT: &str = "passphrase-v1";
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
const MARKER_ERROR: &str = "The app lock marker could not be updated in your data folder.";
fn write_marker(directory: &Path, name: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    std::fs::write(directory.join(name), b"")
}
fn remove_marker(directory: &Path, name: &str) -> std::io::Result<()> {
    match std::fs::remove_file(directory.join(name)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
/// A file in the data folder whose existence cannot be checked counts as `unknown`.
fn file_exists(directory: &Path, name: &str, unknown: bool) -> bool {
    directory.join(name).try_exists().unwrap_or(unknown)
}
/// Keep the markers in step with the stored lock. The markers change first, so a failure
/// to update them leaves the lock in place rather than an unlocked app that still looks locked.
fn change_lock(
    directory: &Path,
    hash: Option<&str>,
    store: impl FnOnce(Option<&str>) -> Result<(), String>,
) -> Result<(), String> {
    let marker_error = |_| MARKER_ERROR.to_string();
    if hash.is_none() {
        remove_marker(directory, LOCK_MARKER).map_err(marker_error)?;
        if let Err(error) = store(None) {
            // The lock is still saved, so it must stay recorded.
            let _ = write_marker(directory, LOCK_MARKER);
            return Err(error);
        }
        // Best effort: without it, keyring-less launches wait for the keyring.
        let _ = write_marker(directory, NO_LOCK_MARKER);
        return Ok(());
    }
    let had_lock = file_exists(directory, LOCK_MARKER, true);
    let had_no_lock = file_exists(directory, NO_LOCK_MARKER, false);
    write_marker(directory, LOCK_MARKER).map_err(marker_error)?;
    remove_marker(directory, NO_LOCK_MARKER).map_err(marker_error)?;
    let stored = store(hash);
    if stored.is_err() {
        // A lock that was never saved must not change how the next launch opens.
        if !had_lock {
            let _ = remove_marker(directory, LOCK_MARKER);
        }
        if had_no_lock {
            let _ = write_marker(directory, NO_LOCK_MARKER);
        }
    }
    stored
}
/// Decide how the app opens from the keyring read and the lock markers in `directory`.
/// A data folder opens without a keyring only when it is recorded as having no lock, or has
/// no database yet. It never opens the database, which would create one.
fn resolve(read: LockRead, directory: &Path) -> Startup {
    match read {
        LockRead::Found(hash) => {
            // An unrecorded lock could open without the keyring later, so it is not settled.
            if write_marker(directory, LOCK_MARKER).is_err()
                || remove_marker(directory, NO_LOCK_MARKER).is_err()
            {
                return Startup::Retry(MARKER_ERROR.into());
            }
            Startup::Locked(hash)
        }
        LockRead::Missing => {
            // Best effort: if either fails, a later keyring-less launch waits for the keyring.
            let _ = remove_marker(directory, LOCK_MARKER);
            let _ = write_marker(directory, NO_LOCK_MARKER);
            Startup::Open { keyring: true }
        }
        LockRead::Unavailable => {
            if file_exists(directory, LOCK_MARKER, true) {
                Startup::KeyringNeeded
            } else if file_exists(directory, NO_LOCK_MARKER, false) {
                Startup::Open { keyring: false }
            } else if !file_exists(directory, DATABASE_FILE, true) {
                let _ = write_marker(directory, NO_LOCK_MARKER);
                Startup::Open { keyring: false }
            } else {
                Startup::KeyringNeeded
            }
        }
        LockRead::Failed(message) => Startup::Retry(message),
    }
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
        let outcome = tokio::task::spawn_blocking(move || resolve(read, &directory))
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
    fn present(directory: &Path, name: &str) -> bool {
        directory.join(name).exists()
    }
    #[test]
    fn keyring_less_profile_reopens_after_saving_a_conversation() {
        let root = data_folder("reopen");
        let directory = root.join("data");
        assert_eq!(
            resolve(LockRead::Unavailable, &directory),
            Startup::Open { keyring: false }
        );
        assert!(present(&directory, NO_LOCK_MARKER));
        // The first saved conversation creates the database.
        std::fs::write(directory.join(DATABASE_FILE), b"").unwrap();
        assert_eq!(
            resolve(LockRead::Unavailable, &directory),
            Startup::Open { keyring: false }
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn profile_created_by_the_cli_opens_without_a_keyring() {
        let directory = data_folder("cli");
        drop(blackwall_core::storage::LocalStore::open(&directory).unwrap());
        assert_eq!(
            resolve(LockRead::Unavailable, &directory),
            Startup::Open { keyring: false }
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn both_markers_need_the_keyring() {
        let directory = data_folder("both");
        std::fs::write(directory.join(LOCK_MARKER), b"").unwrap();
        std::fs::write(directory.join(NO_LOCK_MARKER), b"").unwrap();
        assert_eq!(
            resolve(LockRead::Unavailable, &directory),
            Startup::KeyringNeeded
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn startup_dismissed_prompt_retries_and_existing_data_without_marker_stays_locked() {
        let directory = data_folder("existing");
        assert!(matches!(
            resolve(LockRead::Failed("dismissed".into()), &directory),
            Startup::Retry(_)
        ));
        std::fs::write(directory.join(DATABASE_FILE), b"").unwrap();
        assert_eq!(
            resolve(LockRead::Unavailable, &directory),
            Startup::KeyringNeeded
        );
        assert!(!present(&directory, NO_LOCK_MARKER));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn existing_memories_without_conversations_or_marker_need_the_keyring() {
        let directory = data_folder("memories");
        let mut store = blackwall_core::storage::LocalStore::open(&directory).unwrap();
        store
            .save_memory(&blackwall_core::storage::MemoryEntry {
                id: "memory_one".into(),
                content: "I prefer concise answers".into(),
                updated_at: 1,
                ..Default::default()
            })
            .unwrap();
        drop(store);
        // Data from a version that recorded no lock state.
        let _ = std::fs::remove_file(directory.join(NO_LOCK_MARKER));
        assert_eq!(
            resolve(LockRead::Unavailable, &directory),
            Startup::KeyringNeeded
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn found_lock_records_lock_on_and_missing_lock_records_lock_off() {
        let directory = data_folder("found");
        std::fs::write(directory.join(NO_LOCK_MARKER), b"").unwrap();
        assert_eq!(
            resolve(LockRead::Found("h".into()), &directory),
            Startup::Locked("h".into())
        );
        assert!(present(&directory, LOCK_MARKER));
        assert!(!present(&directory, NO_LOCK_MARKER));
        assert_eq!(
            resolve(LockRead::Missing, &directory),
            Startup::Open { keyring: true }
        );
        assert!(!present(&directory, LOCK_MARKER));
        assert!(present(&directory, NO_LOCK_MARKER));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn found_lock_retries_when_its_markers_cannot_be_updated() {
        // A directory in a marker's place cannot be removed or written as a file.
        let directory = data_folder("found-stuck");
        std::fs::create_dir(directory.join(NO_LOCK_MARKER)).unwrap();
        assert!(matches!(
            resolve(LockRead::Found("h".into()), &directory),
            Startup::Retry(_)
        ));
        std::fs::remove_dir(directory.join(NO_LOCK_MARKER)).unwrap();
        std::fs::remove_file(directory.join(LOCK_MARKER)).unwrap();
        std::fs::create_dir(directory.join(LOCK_MARKER)).unwrap();
        assert!(matches!(
            resolve(LockRead::Found("h".into()), &directory),
            Startup::Retry(_)
        ));
        std::fs::remove_dir_all(directory).unwrap();
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
        std::fs::write(directory.join(NO_LOCK_MARKER), b"").unwrap();
        assert!(change_lock(&directory, Some("hash"), failing).is_err());
        assert!(!marker.exists());
        assert!(
            present(&directory, NO_LOCK_MARKER),
            "a lock that was never saved keeps the no-lock marker"
        );
        std::fs::write(&marker, b"").unwrap();
        assert!(change_lock(&directory, Some("hash"), failing).is_err());
        assert!(marker.exists(), "an existing lock keeps its marker");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn failed_lock_removal_keeps_the_lock_marker() {
        let directory = data_folder("failed-removal");
        std::fs::write(directory.join(LOCK_MARKER), b"").unwrap();
        let failing = |_: Option<&str>| Err("keyring delete failed".to_string());
        assert!(change_lock(&directory, None, failing).is_err());
        assert!(present(&directory, LOCK_MARKER));
        assert!(!present(&directory, NO_LOCK_MARKER));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn lock_is_not_saved_when_the_no_lock_marker_cannot_be_removed() {
        let directory = data_folder("set-stuck");
        std::fs::create_dir(directory.join(NO_LOCK_MARKER)).unwrap();
        let stored = std::cell::Cell::new(false);
        let store = |_: Option<&str>| {
            stored.set(true);
            Ok(())
        };
        assert!(change_lock(&directory, Some("hash"), store).is_err());
        assert!(!stored.get(), "the hash must stay untouched");
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
        std::fs::write(directory.join(NO_LOCK_MARKER), b"").unwrap();
        change_lock(&directory, Some("hash"), store).unwrap();
        assert!(marker.exists());
        assert!(!present(&directory, NO_LOCK_MARKER));
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
        assert!(present(&directory, NO_LOCK_MARKER));
        assert_eq!(stored.get(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
