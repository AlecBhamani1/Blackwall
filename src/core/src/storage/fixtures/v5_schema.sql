CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, title TEXT NOT NULL, updated_at INTEGER NOT NULL, body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS memory (id TEXT PRIMARY KEY, content TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_search USING fts5(id UNINDEXED, content);
CREATE TABLE IF NOT EXISTS paired_devices (id TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS sessions_updated_at ON sessions(updated_at DESC);
ALTER TABLE memory ADD COLUMN scope TEXT NOT NULL DEFAULT 'user';
ALTER TABLE memory ADD COLUMN workspace TEXT NOT NULL DEFAULT '';
ALTER TABLE memory ADD COLUMN fact_key TEXT NOT NULL DEFAULT '';
ALTER TABLE memory ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE memory ADD COLUMN source TEXT;
UPDATE memory SET fact_key=id;
CREATE UNIQUE INDEX memory_fact ON memory(scope,workspace,fact_key);
CREATE TABLE memory_proposals (id TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS indexed_sessions (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            project TEXT, mode TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS session_messages (
            id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            message_id TEXT NOT NULL, message_index INTEGER NOT NULL, role TEXT NOT NULL,
            content TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS session_messages_session ON session_messages(session_id);
CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5(content, content='session_messages', content_rowid='id');
INSERT INTO message_search(message_search, rank) VALUES ('secure-delete', 1);
CREATE TRIGGER IF NOT EXISTS message_insert AFTER INSERT ON session_messages BEGIN
            INSERT INTO message_search(rowid,content) VALUES (new.id,new.content); END;
CREATE TRIGGER IF NOT EXISTS message_delete AFTER DELETE ON session_messages BEGIN
            INSERT INTO message_search(message_search,rowid,content) VALUES ('delete',old.id,old.content); END;
