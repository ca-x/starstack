CREATE TABLE IF NOT EXISTS github_lists (
    owner_id TEXT NOT NULL, list_id TEXT NOT NULL, name TEXT NOT NULL COLLATE NOCASE,
    updated_at TEXT NOT NULL, last_added_at TEXT NOT NULL, item_count BIGINT NOT NULL,
    PRIMARY KEY (owner_id, list_id)
);
CREATE INDEX IF NOT EXISTS github_list_names ON github_lists(owner_id, name);
CREATE TABLE IF NOT EXISTS github_list_members (
    owner_id TEXT NOT NULL, list_id TEXT NOT NULL, repo_id BIGINT NOT NULL,
    PRIMARY KEY (owner_id, list_id, repo_id),
    FOREIGN KEY (owner_id, list_id) REFERENCES github_lists(owner_id, list_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS github_repo_lists ON github_list_members(owner_id, repo_id);
