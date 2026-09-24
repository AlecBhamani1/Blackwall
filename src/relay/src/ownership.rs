//! Durable address ownership. Disconnecting must never make a known URL claimable.
use std::{collections::HashMap, fs, path::Path, sync::Mutex, time::Duration};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

const MAX_OWNERS: usize = 100_000;

#[derive(Debug, Error)]
pub enum OwnershipError {
    #[error("relay ownership storage could not be opened: {0}")]
    Io(#[from] std::io::Error),
    #[error("relay ownership storage failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(&'static str),
}

enum Backend {
    Memory(HashMap<String, ([u8; 32], bool)>),
    Disk(Connection),
}

pub(crate) struct OwnershipStore(Mutex<Backend>);

impl OwnershipStore {
    pub(crate) fn memory() -> Self {
        Self(Mutex::new(Backend::Memory(HashMap::new())))
    }

    pub(crate) fn open(directory: &Path) -> Result<Self, OwnershipError> {
        reject_symlink(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(directory)?;
        let path = directory.join("ownership.sqlite3");
        for suffix in ["", "-journal", "-wal", "-shm"] {
            reject_symlink(&directory.join(format!("ownership.sqlite3{suffix}")))?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 2 {
            return Err(OwnershipError::Invalid(
                "relay ownership data needs a newer relay version",
            ));
        }
        connection.execute_batch(
            "PRAGMA synchronous=FULL;
             BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS owners (
                 session_id TEXT PRIMARY KEY,
                 host_digest BLOB NOT NULL CHECK(length(host_digest)=32)
             );
",
        )?;
        if version < 2 {
            connection.execute_batch(
                "ALTER TABLE owners ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        connection.execute_batch("PRAGMA user_version=2; COMMIT;")?;
        Ok(Self(Mutex::new(Backend::Disk(connection))))
    }

    /// Reserve before acknowledging registration. Retain ownership after expiry too:
    /// an old public URL must not become an address for somebody else's computer.
    pub(crate) fn claim(&self, id: &str, key: &str) -> Result<(), OwnershipError> {
        self.update(id, key, false)
    }
    pub(crate) fn revoke(&self, id: &str, key: &str) -> Result<(), OwnershipError> {
        self.update(id, key, true)
    }
    fn update(&self, id: &str, key: &str, revoke: bool) -> Result<(), OwnershipError> {
        let digest: [u8; 32] = Sha256::new()
            .chain_update(b"blackwall-relay-owner-v1\0")
            .chain_update(id.as_bytes())
            .chain_update(b"\0")
            .chain_update(key.as_bytes())
            .finalize()
            .into();
        let mut backend = self
            .0
            .lock()
            .map_err(|_| OwnershipError::Invalid("relay ownership storage is unavailable"))?;
        match &mut *backend {
            Backend::Memory(owners) => {
                if let Some((existing, revoked)) = owners.get_mut(id) {
                    verify(existing, &digest)?;
                    if *revoked && !revoke {
                        return Err(OwnershipError::Invalid(
                            "This paired device has been revoked.",
                        ));
                    }
                    *revoked |= revoke;
                    return Ok(());
                }
                check_capacity(owners.len())?;
                owners.insert(id.to_owned(), (digest, revoke));
            }
            Backend::Disk(connection) => {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing: Option<(Vec<u8>, bool)> = transaction
                    .query_row(
                        "SELECT host_digest,revoked FROM owners WHERE session_id=?1",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((existing, revoked)) = existing {
                    verify(&existing, &digest)?;
                    if revoked && !revoke {
                        return Err(OwnershipError::Invalid(
                            "This paired device has been revoked.",
                        ));
                    }
                    if revoke {
                        transaction
                            .execute("UPDATE owners SET revoked=1 WHERE session_id=?1", [id])?;
                    }
                    transaction.commit()?;
                    return Ok(());
                }
                let count: u32 =
                    transaction.query_row("SELECT COUNT(*) FROM owners", [], |row| row.get(0))?;
                check_capacity(count as usize)?;
                transaction.execute(
                    "INSERT INTO owners(session_id,host_digest,revoked) VALUES (?1,?2,?3)",
                    params![id, digest.as_slice(), revoke],
                )?;
                transaction.commit()?;
            }
        }
        Ok(())
    }
}

fn verify(existing: &[u8], digest: &[u8; 32]) -> Result<(), OwnershipError> {
    if existing.ct_eq(digest).unwrap_u8() == 1 {
        Ok(())
    } else {
        Err(OwnershipError::Invalid(
            "session identifier is already registered",
        ))
    }
}

fn check_capacity(count: usize) -> Result<(), OwnershipError> {
    if count >= MAX_OWNERS {
        Err(OwnershipError::Invalid(
            "relay address capacity has been reached",
        ))
    } else {
        Ok(())
    }
}

fn reject_symlink(path: &Path) -> Result<(), OwnershipError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(OwnershipError::Invalid(
            "relay ownership storage must not use symbolic links",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn revocation_survives_reopen_and_cannot_be_undone_by_a_stale_owner() {
        let path =
            std::env::temp_dir().join(format!("blackwall-revoked-{}", rand::random::<u64>()));
        let store = OwnershipStore::open(&path).unwrap();
        store.claim("device-one", "first-host").unwrap();
        store.claim("device-two", "second-host").unwrap();
        assert!(store.revoke("device-one", "impostor").is_err());
        store.revoke("device-one", "first-host").unwrap();
        drop(store);
        let store = OwnershipStore::open(&path).unwrap();
        assert!(store.claim("device-one", "first-host").is_err());
        store.revoke("device-one", "first-host").unwrap();
        store.claim("device-two", "second-host").unwrap();
        store.revoke("not-connected-yet", "third-host").unwrap();
        assert!(store.claim("not-connected-yet", "third-host").is_err());
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn ownership_survives_reopen_and_never_stores_raw_keys() {
        let path = std::env::temp_dir().join(format!("blackwall-owners-{}", rand::random::<u64>()));
        let key = "independently-random-host-secret-must-never-be-on-disk";
        let store = OwnershipStore::open(&path).unwrap();
        store.claim("first", key).unwrap();
        drop(store);
        let store = OwnershipStore::open(&path).unwrap();
        store.claim("first", key).unwrap();
        assert!(store.claim("first", "different-host").is_err());
        store.claim("second", "different-host").unwrap();
        drop(store);
        let bytes = fs::read(path.join("ownership.sqlite3")).unwrap();
        assert!(!bytes
            .windows(key.len())
            .any(|value| value == key.as_bytes()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(path.join("ownership.sqlite3"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn address_limit_keeps_existing_owners_working() {
        let store = OwnershipStore::memory();
        if let Backend::Memory(owners) = &mut *store.0.lock().unwrap() {
            for index in 0..MAX_OWNERS {
                owners.insert(index.to_string(), ([0; 32], false));
            }
        }
        assert!(store.claim("new", "key").is_err());
        assert!(store.claim("0", "impostor").is_err());
        let mut backend = store.0.lock().unwrap();
        if let Backend::Memory(owners) = &mut *backend {
            owners.remove("0");
        }
        drop(backend);
        store.claim("last", "key").unwrap();
        store.claim("last", "key").unwrap();
    }

    #[test]
    fn concurrent_database_connections_cannot_claim_the_same_address() {
        let path = std::env::temp_dir().join(format!("blackwall-owners-{}", rand::random::<u64>()));
        let first = OwnershipStore::open(&path).unwrap();
        let second = OwnershipStore::open(&path).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let (one, two) = std::thread::scope(|scope| {
            let one = scope.spawn(|| {
                barrier.wait();
                first.claim("same-address", "first-host").is_ok()
            });
            let two = scope.spawn(|| {
                barrier.wait();
                second.claim("same-address", "second-host").is_ok()
            });
            (one.join().unwrap(), two.join().unwrap())
        });
        assert_ne!(one, two, "exactly one host must win the durable claim");
        drop(first);
        drop(second);
        let reopened = OwnershipStore::open(&path).unwrap();
        assert_eq!(reopened.claim("same-address", "first-host").is_ok(), one);
        assert_eq!(reopened.claim("same-address", "second-host").is_ok(), two);
        drop(reopened);
        fs::remove_dir_all(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_storage_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;
        let path = std::env::temp_dir().join(format!("blackwall-owners-{}", rand::random::<u64>()));
        fs::create_dir(&path).unwrap();
        let target = path.join("target");
        fs::write(&target, b"leave this data alone").unwrap();
        let data = path.join("data");
        fs::create_dir(&data).unwrap();
        symlink(&target, data.join("ownership.sqlite3")).unwrap();
        assert!(OwnershipStore::open(&data).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"leave this data alone");
        let link = path.join("linked-directory");
        symlink(&data, &link).unwrap();
        assert!(OwnershipStore::open(&link).is_err());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn corrupt_and_future_data_are_preserved_and_rejected() {
        let path = std::env::temp_dir().join(format!("blackwall-owners-{}", rand::random::<u64>()));
        fs::create_dir(&path).unwrap();
        let file = path.join("ownership.sqlite3");
        fs::write(&file, b"corrupt existing data").unwrap();
        assert!(OwnershipStore::open(&path).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"corrupt existing data");
        fs::remove_file(&file).unwrap();
        let connection = Connection::open(&file).unwrap();
        connection.pragma_update(None, "user_version", 999).unwrap();
        drop(connection);
        assert!(OwnershipStore::open(&path).is_err());
        fs::remove_dir_all(path).unwrap();
    }
}
