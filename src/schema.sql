CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files (
 id INTEGER PRIMARY KEY, rel TEXT NOT NULL UNIQUE, name TEXT NOT NULL, normal TEXT NOT NULL,
 size INTEGER NOT NULL CHECK(size>=0), mtime INTEGER NOT NULL, identity TEXT NOT NULL,
 links INTEGER NOT NULL, prehash TEXT, hash TEXT, active INTEGER NOT NULL DEFAULT 1,
 cleanable INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_files_size ON files(size,id);
CREATE INDEX IF NOT EXISTS idx_files_pre ON files(size,prehash,id);
CREATE INDEX IF NOT EXISTS idx_files_hash ON files(hash,id);
CREATE TABLE IF NOT EXISTS directories (rel TEXT PRIMARY KEY, name TEXT NOT NULL, depth INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_dir_name ON directories(name,depth,rel);
CREATE TABLE IF NOT EXISTS actions (
 id INTEGER PRIMARY KEY, kind TEXT NOT NULL, source TEXT NOT NULL, target TEXT,
 body TEXT NOT NULL, selected INTEGER NOT NULL, state TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS targets (path TEXT PRIMARY KEY, file_id INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS keepers (file_id INTEGER PRIMARY KEY, hash TEXT NOT NULL, name TEXT NOT NULL, normal TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_keepers_hash ON keepers(hash,name,normal);
CREATE TABLE IF NOT EXISTS events (
 id INTEGER PRIMARY KEY, time TEXT NOT NULL, phase TEXT NOT NULL, source TEXT NOT NULL,
 target TEXT NOT NULL, result TEXT NOT NULL, reason TEXT NOT NULL, size INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS archives (
 id INTEGER PRIMARY KEY, rel TEXT NOT NULL, fingerprint TEXT NOT NULL UNIQUE,
 depth INTEGER NOT NULL, state TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX IF NOT EXISTS idx_actions_source_kind ON actions(source,kind);
CREATE INDEX IF NOT EXISTS idx_files_active ON files(active,id);
CREATE INDEX IF NOT EXISTS idx_actions_state ON actions(state,id);
