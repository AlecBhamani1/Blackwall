//! Reviewed facts. All conflict checks and writes share an immediate SQLite transaction.
use super::{validate_id, LocalStore, StorageError};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    #[default]
    User,
    Project,
    Environment,
}
impl MemoryScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Environment => "environment",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySource {
    pub session_id: String,
    pub message_id: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub updated_at: i64,
    #[serde(default)]
    pub scope: MemoryScope,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub revision: i64,
    #[serde(default)]
    pub source: Option<MemorySource>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalInput {
    pub scope: MemoryScope,
    pub key: String,
    pub content: String,
    /// Existing fact ID from memory_list. Required when replacing a differently keyed fact.
    #[serde(default)]
    pub replaces: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryProposal {
    pub id: String,
    pub entry: MemoryEntry,
    pub before: Option<MemoryEntry>,
    pub source: MemorySource,
    pub created_at: i64,
}
fn clock() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn validate(entry: &MemoryEntry) -> Result<(), StorageError> {
    validate_id(&entry.id)?;
    if entry.content.trim().is_empty() || entry.content.len() > 8000 || entry.updated_at < 0 {
        return Err(StorageError::Invalid(
            "A memory must contain between 1 and 8,000 bytes of text.",
        ));
    }
    if entry.key.is_empty() || entry.key.len() > 128 || entry.key.chars().any(char::is_control) {
        return Err(StorageError::Invalid(
            "Use a fact key between 1 and 128 bytes.",
        ));
    }
    if (entry.scope == MemoryScope::User && !entry.workspace.is_empty())
        || (entry.scope != MemoryScope::User
            && (!std::path::Path::new(&entry.workspace).is_absolute()
                || entry.workspace.len() > 4096))
    {
        return Err(StorageError::Invalid("Project and environment memories need an absolute project folder; user preferences have no project folder."));
    }
    Ok(())
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let scope: String = r.get(3)?;
    Ok(MemoryEntry {
        id: r.get(0)?,
        content: r.get(1)?,
        updated_at: r.get(2)?,
        scope: match scope.as_str() {
            "project" => MemoryScope::Project,
            "environment" => MemoryScope::Environment,
            _ => MemoryScope::User,
        },
        workspace: r.get(4)?,
        key: r.get(5)?,
        revision: r.get(6)?,
        source: r
            .get::<_, Option<String>>(7)?
            .map(|s| {
                serde_json::from_str(&s).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .transpose()?,
    })
}
const COLUMNS: &str = "id,content,updated_at,scope,workspace,fact_key,revision,source";
fn get(db: &rusqlite::Connection, id: &str) -> Result<Option<MemoryEntry>, StorageError> {
    Ok(db
        .query_row(
            &format!("SELECT {COLUMNS} FROM memory WHERE id=?1"),
            [id],
            row,
        )
        .optional()?)
}
fn write(db: &rusqlite::Connection, entry: &MemoryEntry) -> Result<(), StorageError> {
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM memory WHERE id!=?1",
        [&entry.id],
        |r| r.get(0),
    )?;
    if count >= 2000 {
        return Err(StorageError::Invalid(
            "Memory is full. Remove an older memory first.",
        ));
    }
    // Keep the stable fact identity when users edit text. Provenance is assigned by the runtime.
    db.execute("INSERT INTO memory(id,content,updated_at,scope,workspace,fact_key,revision,source) VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET content=excluded.content,updated_at=excluded.updated_at,revision=excluded.revision,source=excluded.source", params![entry.id,entry.content,entry.updated_at,entry.scope.label(),entry.workspace,entry.key,entry.revision,entry.source.as_ref().map(serde_json::to_string).transpose()?])?;
    db.execute("DELETE FROM memory_search WHERE id=?1", [&entry.id])?;
    db.execute(
        "INSERT INTO memory_search(id,content) VALUES (?1,?2)",
        [&entry.id, &entry.content],
    )?;
    Ok(())
}
fn next_revision(db: &rusqlite::Connection) -> Result<i64, StorageError> {
    let saved: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='memory-revision'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let previous = saved
        .map(|s| {
            s.parse::<i64>()
                .map_err(|_| StorageError::Invalid("Invalid memory revision."))
        })
        .transpose()?
        .unwrap_or(1);
    let next = previous
        .checked_add(1)
        .ok_or(StorageError::Invalid("Memory revision limit reached."))?;
    db.execute("INSERT INTO settings(key,value) VALUES ('memory-revision',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [next.to_string()])?;
    Ok(next)
}
const STALE: &str =
    "This memory changed or was forgotten. Refresh and review a new proposal before saving.";
impl LocalStore {
    pub fn memories(&self, query: &str) -> Result<Vec<MemoryEntry>, StorageError> {
        if query.len() > 1024 {
            return Err(StorageError::Invalid("Use a shorter memory search."));
        }
        let phrase = query
            .split_whitespace()
            .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = if phrase.is_empty() {
            format!("SELECT {COLUMNS} FROM memory ORDER BY updated_at DESC,id LIMIT 2000")
        } else {
            format!("SELECT {COLUMNS} FROM memory WHERE id IN (SELECT id FROM memory_search WHERE memory_search MATCH ?1) ORDER BY updated_at DESC,id LIMIT 50")
        };
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = if phrase.is_empty() {
            statement.query([])?
        } else {
            statement.query([phrase])?
        };
        let mut entries = Vec::new();
        while let Some(r) = rows.next()? {
            entries.push(row(r)?);
        }
        Ok(entries)
    }
    /// Owner management remains available while injection/agent learning is switched off.
    pub fn save_memory(&mut self, entry: &MemoryEntry) -> Result<(), StorageError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = get(&tx, &entry.id)?;
        if previous.as_ref().map_or(0, |e| e.revision) != entry.revision {
            return Err(StorageError::Invalid(STALE));
        }
        let mut updated = entry.clone();
        if let Some(previous) = previous {
            updated.scope = previous.scope;
            updated.workspace = previous.workspace;
            updated.key = previous.key;
            updated.source = previous.source;
        } else {
            updated.key = if updated.key.is_empty() {
                updated.id.clone()
            } else {
                normalized(&updated.key)
            };
            updated.source = None;
        }
        validate(&updated)?;
        updated.updated_at = clock();
        updated.revision = next_revision(&tx)?;
        write(&tx, &updated)?;
        tx.commit()?;
        Ok(())
    }
    pub fn delete_memory(&mut self, id: &str) -> Result<(), StorageError> {
        validate_id(id)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Forget also discards proposals for this fact, preventing accidental resurrection.
        tx.execute("DELETE FROM memory_proposals WHERE json_extract(body,'$.entry.id')=?1 OR json_extract(body,'$.before.id')=?1", [id])?;
        tx.execute("DELETE FROM memory_search WHERE id=?1", [id])?;
        tx.execute("DELETE FROM memory WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn memory_proposals(&self) -> Result<Vec<MemoryProposal>, StorageError> {
        let mut statement = self
            .connection
            .prepare("SELECT body FROM memory_proposals ORDER BY rowid DESC LIMIT 200")?;
        let result = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|body| Ok(serde_json::from_str(&body?)?))
            .collect();
        result
    }
    pub fn reject_memory(&self, id: &str) -> Result<(), StorageError> {
        validate_id(id)?;
        self.connection
            .execute("DELETE FROM memory_proposals WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn approve_memory(&mut self, id: &str, content: Option<&str>) -> Result<(), StorageError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let body: String = tx
            .query_row("SELECT body FROM memory_proposals WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or(StorageError::Invalid(
                "This proposal is no longer pending. Refresh the review queue.",
            ))?;
        let proposal: MemoryProposal = serde_json::from_str(&body)?;
        let current = get(&tx, &proposal.entry.id)?;
        if current.as_ref().map(|e| e.revision) != proposal.before.as_ref().map(|e| e.revision) {
            return Err(StorageError::Invalid(STALE));
        }
        // A competing new proposal may have claimed the same scope/key with another ID.
        let occupied: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory WHERE scope=?1 AND workspace=?2 AND fact_key=?3 AND id!=?4)", params![proposal.entry.scope.label(),proposal.entry.workspace,proposal.entry.key,proposal.entry.id], |r| r.get(0))?;
        if occupied {
            return Err(StorageError::Invalid(STALE));
        }
        let mut entry = proposal.entry;
        if let Some(content) = content {
            entry.content = content.trim().into();
        }
        entry.updated_at = clock();
        entry.revision = next_revision(&tx)?;
        entry.source = Some(proposal.source);
        validate(&entry)?;
        let mut statement =
            tx.prepare("SELECT content FROM memory WHERE scope=?1 AND workspace=?2 AND id!=?3")?;
        let duplicate = statement
            .query_map(
                params![entry.scope.label(), entry.workspace, entry.id],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|content| normalized(content) == normalized(&entry.content));
        drop(statement);
        if duplicate {
            return Err(StorageError::Invalid(
                "This fact was already saved. Reject this duplicate proposal.",
            ));
        }
        write(&tx, &entry)?;
        tx.execute("DELETE FROM memory_proposals WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }
    /// Called only with runtime-supplied owner access and source, never model-supplied provenance.
    pub(crate) fn propose_memory(
        &mut self,
        input: ProposalInput,
        workspace: Option<&str>,
        source: MemorySource,
        client: crate::memory::MemoryClient,
    ) -> Result<MemoryProposal, StorageError> {
        validate_id(&source.session_id)?;
        validate_id(&source.message_id)?;
        let workspace = if input.scope == MemoryScope::User {
            ""
        } else {
            workspace.ok_or(StorageError::Invalid(
                "Choose a project before proposing project or environment facts.",
            ))?
        };
        let key = normalized(&input.key);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !crate::memory::enabled(&tx, client)? {
            return Err(StorageError::Invalid(
                "Memory is disabled or unavailable in this chat.",
            ));
        }
        let session: String = tx
            .query_row(
                "SELECT body FROM sessions WHERE id=?1",
                [&source.session_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(StorageError::Invalid(
                "Save the source conversation before proposing a memory.",
            ))?;
        let session: serde_json::Value = serde_json::from_str(&session)?;
        let has_source = session["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["id"].as_str() == Some(&source.message_id) && message["role"] == "user"
            })
        });
        if !has_source
            || (input.scope != MemoryScope::User
                && session["workspace"].as_str() != Some(workspace))
        {
            return Err(StorageError::Invalid(
                "This proposal has no matching source message or project.",
            ));
        }
        let before = if let Some(id) = input.replaces {
            let entry = get(&tx, &id)?.ok_or(StorageError::Invalid(STALE))?;
            if entry.scope != input.scope || entry.workspace != workspace {
                return Err(StorageError::Invalid(
                    "A replacement must stay in the same scope and project.",
                ));
            }
            Some(entry)
        } else {
            tx.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM memory WHERE scope=?1 AND workspace=?2 AND fact_key=?3"
                ),
                params![input.scope.label(), workspace, key],
                row,
            )
            .optional()?
        };
        let id = format!("learned_{:016x}", rand::random::<u64>());
        let entry = MemoryEntry {
            id: before.as_ref().map_or_else(|| id.clone(), |e| e.id.clone()),
            content: input.content.trim().into(),
            updated_at: clock(),
            scope: input.scope,
            workspace: workspace.into(),
            key: before.as_ref().map_or(key, |e| e.key.clone()),
            revision: before.as_ref().map_or(0, |e| e.revision),
            source: None,
        };
        validate(&entry)?;
        // Content duplicates under another key do not create a second durable fact.
        let mut statement =
            tx.prepare("SELECT content FROM memory WHERE scope=?1 AND workspace=?2")?;
        let duplicate = statement
            .query_map(params![entry.scope.label(), workspace], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|c| normalized(c) == normalized(&entry.content));
        drop(statement);
        if duplicate {
            return Err(StorageError::Invalid("This fact is already saved."));
        }
        let mut statement = tx.prepare("SELECT body FROM memory_proposals")?;
        let pending = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for body in &pending {
            let proposal: MemoryProposal = serde_json::from_str(body)?;
            if proposal.entry.scope == entry.scope
                && proposal.entry.workspace == entry.workspace
                && (proposal.entry.key == entry.key
                    || normalized(&proposal.entry.content) == normalized(&entry.content))
            {
                return Err(StorageError::Invalid(
                    "A related proposal is already pending. Review or reject it first.",
                ));
            }
        }
        if pending.len() >= 200 {
            return Err(StorageError::Invalid(
                "Review pending memories before proposing more.",
            ));
        }
        let proposal = MemoryProposal {
            id: format!("proposal_{:016x}", rand::random::<u64>()),
            entry,
            before,
            source,
            created_at: clock(),
        };
        tx.execute(
            "INSERT INTO memory_proposals(id,body) VALUES (?1,?2)",
            params![proposal.id, serde_json::to_string(&proposal)?],
        )?;
        tx.commit()?;
        Ok(proposal)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::memory::MemoryClient;
    use serde_json::json;
    // Native absolute paths include the drive prefix required on Windows.
    fn workspace(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("blackwall-memory-test-{name}"))
            .to_string_lossy()
            .into_owned()
    }
    fn store() -> LocalStore {
        let store =
            LocalStore::initialize(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        store
            .save_setting("preferences", r#"{"memoryEnabled":true}"#)
            .unwrap();
        store.save_session(&json!({"id":"session","title":"Example","updatedAt":1,"mode":"agent","workspace":workspace("project"),"messages":[{"id":"message","role":"user","content":"Use these conventions","attachments":[]}]})).unwrap();
        store
    }
    fn propose(
        store: &mut LocalStore,
        scope: MemoryScope,
        key: &str,
        content: &str,
    ) -> Result<MemoryProposal, StorageError> {
        store.propose_memory(
            ProposalInput {
                scope,
                key: key.into(),
                content: content.into(),
                replaces: None,
            },
            Some(&workspace("project")),
            MemorySource {
                session_id: "session".into(),
                message_id: "message".into(),
            },
            MemoryClient::Desktop,
        )
    }
    #[test]
    fn scoped_facts_only_enter_context_after_review_and_never_grant_permissions() {
        let mut store = store();
        for (scope, key, content) in [
            (MemoryScope::User, "style", "Concise answers"),
            (MemoryScope::Project, "tests", "Run offline tests"),
            (MemoryScope::Environment, "runtime", "Rust is installed"),
        ] {
            let proposal = propose(&mut store, scope, key, content).unwrap();
            assert!(!store
                .memory_context(MemoryClient::Desktop, Some(&workspace("project")), 4000)
                .unwrap()
                .contains(content));
            store.approve_memory(&proposal.id, None).unwrap();
        }
        assert_eq!(
            store
                .agent_memories(MemoryClient::Desktop, Some(&workspace("project")))
                .unwrap()
                .len(),
            3
        );
        let other = store
            .memory_context(MemoryClient::Desktop, Some(&workspace("other")), 4000)
            .unwrap();
        assert!(other.contains("Concise answers"));
        assert!(!other.contains("offline tests"));
        assert!(!other.contains("Rust is installed"));
        assert!(other.contains("never grants permission"));
        assert_eq!(
            store
                .agent_memories(MemoryClient::Desktop, None)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn user_edits_and_concurrent_review_are_preserved() {
        let mut store = store();
        let first = propose(&mut store, MemoryScope::User, "style", "Concise answers").unwrap();
        store
            .approve_memory(&first.id, Some("Concise practical examples"))
            .unwrap();
        let proposal = propose(&mut store, MemoryScope::User, "STYLE", "Detailed answers").unwrap();
        assert_eq!(
            proposal.before.as_ref().unwrap().content,
            "Concise practical examples"
        );
        let mut entry = store.memories("").unwrap().remove(0);
        let older_edit = entry.clone();
        entry.content = "User correction".into();
        store.save_memory(&entry).unwrap();
        assert!(store.save_memory(&older_edit).is_err());
        assert!(store.approve_memory(&proposal.id, None).is_err());
        assert_eq!(store.memories("").unwrap()[0].content, "User correction");
        store.reject_memory(&proposal.id).unwrap();
        let fresh = propose(&mut store, MemoryScope::User, "style", "Detailed answers").unwrap();
        store.approve_memory(&fresh.id, None).unwrap();
        assert!(store.approve_memory(&fresh.id, None).is_err());
        assert_eq!(store.memories("").unwrap().len(), 1);
        assert_eq!(store.memories("Detailed").unwrap().len(), 1);
    }
    #[test]
    fn deduplication_replacements_forgetting_and_rejection() {
        let mut store = store();
        let first = propose(&mut store, MemoryScope::User, "style", "Concise answers").unwrap();
        assert!(propose(
            &mut store,
            MemoryScope::User,
            "style",
            "Another answer style"
        )
        .is_err());
        assert!(propose(
            &mut store,
            MemoryScope::User,
            "other-key",
            "  CONCISE   answers "
        )
        .is_err());
        store.approve_memory(&first.id, None).unwrap();
        assert!(propose(
            &mut store,
            MemoryScope::User,
            "other-key",
            "  CONCISE   answers "
        )
        .is_err());
        let replacement = store
            .propose_memory(
                ProposalInput {
                    scope: MemoryScope::User,
                    key: "different-topic".into(),
                    content: "Detailed answers".into(),
                    replaces: Some(first.entry.id.clone()),
                },
                Some(&workspace("project")),
                first.source.clone(),
                MemoryClient::Desktop,
            )
            .unwrap();
        assert_eq!(replacement.entry.key, "style");
        store.delete_memory(&first.entry.id).unwrap();
        assert!(store.memory_proposals().unwrap().is_empty());
        assert!(store.approve_memory(&replacement.id, None).is_err());
        assert!(store.memories("").unwrap().is_empty());
        assert!(store.memories("Concise").unwrap().is_empty());
        let pending = propose(&mut store, MemoryScope::User, "style", "Concise answers").unwrap();
        store.reject_memory(&pending.id).unwrap();
        assert!(store.memory_proposals().unwrap().is_empty());
        assert!(store.memories("").unwrap().is_empty());
    }
    #[test]
    fn guest_disabled_memory_and_invalid_provenance_cannot_learn() {
        let mut store = store();
        let input = ProposalInput {
            scope: MemoryScope::User,
            key: "style".into(),
            content: "Preference".into(),
            replaces: None,
        };
        let source = MemorySource {
            session_id: "session".into(),
            message_id: "message".into(),
        };
        assert!(store
            .propose_memory(input.clone(), None, source.clone(), MemoryClient::Guest)
            .is_err());
        let pending = propose(&mut store, MemoryScope::User, "style", "Preference").unwrap();
        store.approve_memory(&pending.id, None).unwrap();
        assert!(store
            .agent_memories(MemoryClient::Guest, Some(&workspace("project")))
            .unwrap()
            .is_empty());
        store
            .save_setting("preferences", r#"{"memoryEnabled":false}"#)
            .unwrap();
        assert!(store
            .memory_context(MemoryClient::Desktop, Some(&workspace("project")), 4000)
            .unwrap()
            .is_empty());
        assert!(propose(&mut store, MemoryScope::User, "new", "Other fact").is_err());
        // Owner review and forgetting still work when learning is disabled.
        assert_eq!(store.memories("").unwrap().len(), 1);
        store
            .save_setting("preferences", r#"{"memoryEnabled":true}"#)
            .unwrap();
        store
            .save_setting("cli-settings", r#"{"memoryEnabled":false}"#)
            .unwrap();
        assert!(store
            .agent_memories(MemoryClient::Cli, None)
            .unwrap()
            .is_empty());
        assert!(!store
            .agent_memories(MemoryClient::Desktop, None)
            .unwrap()
            .is_empty());
        let invalid = MemorySource {
            session_id: "session".into(),
            message_id: "invented".into(),
        };
        assert!(store
            .propose_memory(
                input,
                Some(&workspace("project")),
                invalid,
                MemoryClient::Desktop
            )
            .is_err());
        assert!(store
            .propose_memory(
                ProposalInput {
                    scope: MemoryScope::Project,
                    key: "test".into(),
                    content: "Fact".into(),
                    replaces: None
                },
                Some(&workspace("other")),
                source,
                MemoryClient::Desktop
            )
            .is_err());
    }
    #[test]
    fn accepted_and_pending_entries_survive_restart_and_old_schema_migrates() {
        let directory =
            std::env::temp_dir().join(format!("blackwall-learning-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&directory).unwrap();
        let connection = rusqlite::Connection::open(directory.join("blackwall.sqlite3")).unwrap();
        connection.execute_batch("CREATE TABLE memory(id TEXT PRIMARY KEY,content TEXT NOT NULL,updated_at INTEGER NOT NULL); INSERT INTO memory VALUES ('legacy','Old user preference',1); PRAGMA user_version=2;").unwrap();
        drop(connection);
        let mut store = LocalStore::open(&directory).unwrap();
        assert_eq!(store.memories("").unwrap()[0].scope, MemoryScope::User);
        assert_eq!(store.memories("").unwrap()[0].key, "legacy");
        store
            .save_setting("preferences", r#"{"memoryEnabled":true}"#)
            .unwrap();
        store.save_session(&json!({"id":"session","title":"Example","updatedAt":1,"workspace":workspace("project"),"messages":[{"id":"message","role":"user","content":"Convention","attachments":[]}]})).unwrap();
        let accepted = propose(&mut store, MemoryScope::Project, "tests", "Run tests").unwrap();
        store.approve_memory(&accepted.id, None).unwrap();
        let pending = propose(&mut store, MemoryScope::User, "style", "Concise answers").unwrap();
        drop(store);
        let mut store = LocalStore::open(&directory).unwrap();
        assert_eq!(store.memories("").unwrap().len(), 2);
        assert_eq!(store.memory_proposals().unwrap()[0].id, pending.id);
        assert_eq!(
            store.memories("tests").unwrap()[0].source.as_ref().unwrap(),
            &accepted.source
        );
        store.delete_memory(&accepted.entry.id).unwrap();
        drop(store);
        assert!(LocalStore::open(&directory)
            .unwrap()
            .memories("tests")
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn concurrent_database_opens_and_reviews_preserve_newer_user_edits() {
        let directory = std::env::temp_dir().join(format!(
            "blackwall-concurrent-learning-{}",
            rand::random::<u64>()
        ));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers = (0..2)
            .map(|_| {
                let directory = directory.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    LocalStore::open(&directory).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        let mut desktop = LocalStore::open(&directory).unwrap();
        desktop
            .save_memory(&MemoryEntry {
                id: "fact".into(),
                content: "Original user preference".into(),
                ..Default::default()
            })
            .unwrap();
        desktop
            .save_setting("preferences", r#"{"memoryEnabled":true}"#)
            .unwrap();
        desktop.save_session(&json!({"id":"session","title":"Example","updatedAt":1,"workspace":workspace("project"),"messages":[{"id":"message","role":"user","content":"Correction","attachments":[]}]})).unwrap();
        let proposal = propose(
            &mut desktop,
            MemoryScope::User,
            "fact",
            "Proposed correction",
        )
        .unwrap();
        let mut cli = LocalStore::open(&directory).unwrap();
        let old = cli.memories("").unwrap().remove(0);
        let mut edit = old.clone();
        edit.content = "Newer user edit".into();
        cli.save_memory(&edit).unwrap();
        assert!(desktop.approve_memory(&proposal.id, None).is_err());
        assert!(desktop.save_memory(&old).is_err());
        assert_eq!(desktop.memories("").unwrap()[0].content, "Newer user edit");
        // Recreating an ID cannot make an old revision valid again.
        cli.delete_memory("fact").unwrap();
        cli.save_memory(&MemoryEntry {
            id: "fact".into(),
            content: "Recreated fact".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(desktop.save_memory(&old).is_err());
        drop(cli);
        drop(desktop);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
