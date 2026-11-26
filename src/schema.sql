CREATE TABLE IF NOT EXISTS repositories (
    owner_id TEXT NOT NULL,
    id INTEGER NOT NULL,
    metadata TEXT NOT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    note TEXT NOT NULL DEFAULT '',
    pinned BOOLEAN NOT NULL DEFAULT 0,
    active BOOLEAN NOT NULL DEFAULT 1,
    PRIMARY KEY(owner_id,id)
);
CREATE INDEX IF NOT EXISTS library_active ON repositories(owner_id,active);
CREATE TABLE IF NOT EXISTS metadata_values (
    owner_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(owner_id,key)
);
CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY NOT NULL, username TEXT UNIQUE NOT NULL, password_hash TEXT NOT NULL,
    admin BOOLEAN NOT NULL, disabled BOOLEAN NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS access_tokens (
    id TEXT PRIMARY KEY NOT NULL, owner_id TEXT NOT NULL REFERENCES accounts(id), name TEXT NOT NULL,
    digest TEXT UNIQUE NOT NULL, created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS access_tokens_owner ON access_tokens(owner_id);
