CREATE TABLE IF NOT EXISTS commit_tracking (
    owner_id TEXT NOT NULL,
    repo_id INTEGER NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY (owner_id, repo_id),
    FOREIGN KEY (owner_id, repo_id) REFERENCES repositories(owner_id, id)
);
CREATE TABLE IF NOT EXISTS repository_commits (
    owner_id TEXT NOT NULL,
    repo_id INTEGER NOT NULL,
    sha TEXT NOT NULL,
    committed_at TEXT NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (owner_id, repo_id, sha),
    FOREIGN KEY (owner_id, repo_id) REFERENCES repositories(owner_id, id)
);
CREATE INDEX IF NOT EXISTS recent_repository_commits
    ON repository_commits(owner_id, repo_id, committed_at DESC, sha DESC);
