#![allow(clippy::unwrap_used)]
use super::*;
use crate::auth::AuthState;
use blackwall_core::{pairing::*, storage::LocalStore};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;

const KEY: u8 = 1;
const PENDING: u8 = 2;
const RELAY_BEFORE: u8 = 3;
const RELAY_AFTER: u8 = 4;
const ACTIVE: u8 = 5;
const REMOVE_MARK: u8 = 6;
const REMOVE_KEY: u8 = 7;
const REMOVE_RECORD: u8 = 8;

struct TestStore {
    directory: PathBuf,
    auth: AuthState,
    keys: Mutex<HashMap<String, String>>,
    receipts: Mutex<HashMap<String, String>>,
    fail: AtomicU8,
    pause: AtomicU8,
    entered: Notify,
    resume: Notify,
}
impl Drop for TestStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl TestStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            directory: std::env::temp_dir()
                .join(format!("blackwall-pair-fault-{}", secret("bwd_"))),
            auth: AuthState::unlocked_for_test(),
            keys: Mutex::new(HashMap::new()),
            receipts: Mutex::new(HashMap::new()),
            fail: AtomicU8::new(0),
            pause: AtomicU8::new(0),
            entered: Notify::new(),
            resume: Notify::new(),
        })
    }
    async fn await_entry(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), self.entered.notified())
            .await
            .unwrap();
    }
    fn records(&self) -> Vec<PairedDevice> {
        LocalStore::open(&self.directory)
            .unwrap()
            .paired_devices()
            .unwrap()
    }
    fn failure(&self, stage: u8) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) == stage {
            Err("Injected operation failure".into())
        } else {
            Ok(())
        }
    }
    async fn pause_at(&self, stage: u8) {
        if self.pause.load(Ordering::SeqCst) == stage {
            self.entered.notify_one();
            self.resume.notified().await;
        }
    }
    async fn publish(&self, device: &PairedDevice) -> Result<(), String> {
        self.failure(RELAY_BEFORE)?;
        // Model access must not be restorable at the point a relay sees consent.
        assert!(self
            .records()
            .iter()
            .any(|row| row.id == device.id && row.state == "pending"));
        self.receipts
            .lock()
            .unwrap()
            .insert(device.id.clone(), device.hash.clone());
        self.pause_at(RELAY_AFTER).await;
        self.failure(RELAY_AFTER)
    }
    async fn approve(&self, device: &PairedDevice, key: &str) -> Result<(), String> {
        approve_host(self, device, key, self.publish(device)).await
    }
}
impl DeviceStore for TestStore {
    async fn ensure_unlocked(&self) -> Result<(), String> {
        self.auth.ensure_unlocked().await
    }
    async fn write_key(&self, device: &PairedDevice, key: &str) -> Result<(), String> {
        self.failure(KEY)?;
        self.keys
            .lock()
            .unwrap()
            .insert(device.endpoint(), key.to_owned());
        self.pause_at(KEY).await;
        Ok(())
    }
    async fn write_pending(&self, device: &PairedDevice) -> Result<(), String> {
        self.failure(PENDING)?;
        LocalStore::open(&self.directory)
            .unwrap()
            .save_paired_device(device)
            .unwrap();
        self.pause_at(PENDING).await;
        Ok(())
    }
    async fn commit_active(&self, device: &PairedDevice) -> Result<(), String> {
        let authorization = self.auth.authorize_commit().await?;
        self.failure(ACTIVE)?;
        self.pause_at(ACTIVE).await;
        let mut active = device.clone();
        active.state = "active".into();
        LocalStore::open(&self.directory)
            .unwrap()
            .save_paired_device(&active)
            .unwrap();
        drop(authorization);
        Ok(())
    }
}
fn device(role: &str) -> (PairedDevice, String) {
    let key = secret(if role == "host" { "bwh_" } else { "bw1_" });
    let (salt, hash) = digest(&secret("bw1_"));
    (
        PairedDevice {
            id: secret("bws_"),
            role: role.into(),
            name: "Home computer".into(),
            relay_url: "https://pairing-tests.invalid".into(),
            model: "test-model".into(),
            upstream: (role == "host").then(|| "http://127.0.0.1:11434/v1".into()),
            salt,
            hash,
            state: if role == "host" { "pending" } else { "active" }.into(),
        },
        key,
    )
}

impl RemovalStore for TestStore {
    async fn mark_removing(&self, device: &PairedDevice) -> Result<(), String> {
        self.auth.ensure_unlocked().await?;
        self.failure(REMOVE_MARK)?;
        LocalStore::open(&self.directory)
            .unwrap()
            .save_paired_device(device)
            .map_err(|error| error.to_string())
    }
    async fn delete_key(&self, device: &PairedDevice) -> Result<(), String> {
        self.failure(REMOVE_KEY)?;
        self.keys.lock().unwrap().remove(&device.endpoint());
        self.pause_at(REMOVE_KEY).await;
        Ok(())
    }
    async fn delete_record(&self, device: &PairedDevice) -> Result<(), String> {
        self.auth.ensure_unlocked().await?;
        self.failure(REMOVE_RECORD)?;
        LocalStore::open(&self.directory)
            .unwrap()
            .delete_paired_device(&device.id)
            .map_err(|error| error.to_string())
    }
}

#[tokio::test]
async fn failed_local_removal_keeps_durable_intent_and_retries_without_an_active_missing_key() {
    for failure in [REMOVE_MARK, REMOVE_KEY, REMOVE_RECORD] {
        let store = TestStore::new();
        let (device, key) = device("client");
        save_client(store.as_ref(), &device, &key).await.unwrap();
        store.fail.store(failure, Ordering::SeqCst);
        assert!(remove_local(store.as_ref(), &device).await.is_err());
        let persisted = store.records().pop().unwrap();
        assert_eq!(
            persisted.state,
            if failure == REMOVE_MARK {
                "active"
            } else {
                "revoking"
            }
        );
        assert_eq!(
            store.keys.lock().unwrap().contains_key(&device.endpoint()),
            failure != REMOVE_RECORD
        );
        store.fail.store(0, Ordering::SeqCst);
        remove_local(store.as_ref(), &persisted).await.unwrap();
        assert!(store.records().is_empty());
        assert!(store.keys.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn interrupted_local_removal_keeps_retryable_intent_after_key_deletion() {
    let store = TestStore::new();
    let (device, key) = device("client");
    save_client(store.as_ref(), &device, &key).await.unwrap();
    store.pause.store(REMOVE_KEY, Ordering::SeqCst);
    let worker = store.clone();
    let caller = tokio::spawn(async move { remove_local(worker.as_ref(), &device).await });
    store.await_entry().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let persisted = store.records().pop().unwrap();
    assert_eq!(persisted.state, "revoking");
    assert!(store.keys.lock().unwrap().is_empty());
    store.pause.store(0, Ordering::SeqCst);
    remove_local(store.as_ref(), &persisted).await.unwrap();
    assert!(store.records().is_empty());
}

#[tokio::test]
async fn host_write_and_relay_failures_retry_one_identity_without_restoring_pending_access() {
    for failure in [KEY, PENDING, RELAY_BEFORE, RELAY_AFTER, ACTIVE] {
        let store = TestStore::new();
        let (device, key) = device("host");
        store.fail.store(failure, Ordering::SeqCst);
        assert!(store.approve(&device, &key).await.is_err());
        let rows = store.records(); // Reopens real SQLite, without relying on an in-memory record.
        assert!(rows.iter().all(|row| row.state == "pending"));
        assert_eq!(rows.len(), usize::from(failure >= RELAY_BEFORE));
        assert_eq!(
            store.keys.lock().unwrap().len(),
            usize::from(failure != KEY)
        );
        for row in rows {
            assert!(blackwall_core::share::ShareHub::new()
                .start_paired(&row, key.clone(), None, None)
                .await
                .is_err());
        }
        store.fail.store(0, Ordering::SeqCst);
        store.approve(&device, &key).await.unwrap();
        let rows = store.records();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, device.id);
        assert_eq!(rows[0].state, "active");
        assert_eq!(store.keys.lock().unwrap().len(), 1);
        assert_eq!(store.receipts.lock().unwrap().len(), 1);
        let bytes = std::fs::read(store.directory.join("blackwall.sqlite3")).unwrap();
        assert!(!bytes
            .windows(key.len())
            .any(|window| window == key.as_bytes()));
    }
}

#[tokio::test]
async fn locking_during_key_save_pending_save_or_relay_approval_prevents_activation() {
    for stage in [KEY, PENDING, RELAY_AFTER] {
        let store = TestStore::new();
        let (device, key) = device("host");
        store.pause.store(stage, Ordering::SeqCst);
        let task_store = store.clone();
        let task = tokio::spawn(async move { task_store.approve(&device, &key).await });
        store.await_entry().await;
        store.auth.begin_lock().await.unwrap();
        store.resume.notify_one();
        assert!(task.await.unwrap().is_err());
        assert!(store.records().iter().all(|row| row.state == "pending"));
    }
}

#[tokio::test]
async fn abandoned_approval_leaves_only_recoverable_keys_or_pending_records() {
    for stage in [KEY, PENDING, RELAY_AFTER] {
        let store = TestStore::new();
        let (device, key) = device("host");
        store.pause.store(stage, Ordering::SeqCst);
        let task_store = store.clone();
        let task = tokio::spawn(async move { task_store.approve(&device, &key).await });
        store.await_entry().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(store.records().iter().all(|row| row.state == "pending"));
    }
}

#[tokio::test]
async fn client_failures_and_locking_never_report_a_saved_connection() {
    for failure in [KEY, ACTIVE] {
        let store = TestStore::new();
        let (device, key) = device("client");
        store.fail.store(failure, Ordering::SeqCst);
        assert!(save_client(&*store, &device, &key).await.is_err());
        assert!(store.records().is_empty());
        store.fail.store(0, Ordering::SeqCst);
        save_client(&*store, &device, &key).await.unwrap();
        assert_eq!(store.records().len(), 1);
        assert_eq!(store.records()[0].id, device.id);
    }
    let store = TestStore::new();
    let (device, key) = device("client");
    store.pause.store(KEY, Ordering::SeqCst);
    let task_store = store.clone();
    let task = tokio::spawn(async move { save_client(&*task_store, &device, &key).await });
    store.await_entry().await;
    store.auth.begin_lock().await.unwrap();
    store.resume.notify_one();
    assert!(task.await.unwrap().is_err());
    assert!(store.records().is_empty());
}

#[tokio::test]
async fn activation_already_committing_finishes_before_lock_can_begin() {
    let store = TestStore::new();
    let (device, key) = device("host");
    store.pause.store(ACTIVE, Ordering::SeqCst);
    let task_store = store.clone();
    let approval = tokio::spawn(async move { task_store.approve(&device, &key).await });
    store.await_entry().await;
    let lock_store = store.clone();
    let lock = tokio::spawn(async move { lock_store.auth.begin_lock().await });
    tokio::task::yield_now().await;
    assert!(!lock.is_finished());
    assert_eq!(store.records()[0].state, "pending");
    store.resume.notify_one();
    approval.await.unwrap().unwrap();
    lock.await.unwrap().unwrap();
    assert_eq!(store.records()[0].state, "active");
    assert!(store.auth.ensure_unlocked().await.is_err());
}
