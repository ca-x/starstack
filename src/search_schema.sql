ALTER TABLE repositories ADD COLUMN source_version INTEGER NOT NULL DEFAULT 0;
CREATE TABLE index_source_versions (
    owner TEXT NOT NULL, id INTEGER NOT NULL, version INTEGER NOT NULL, PRIMARY KEY(owner,id)
);

CREATE VIRTUAL TABLE repository_fts USING fts5(
    owner UNINDEXED, repo_id UNINDEXED, name, body, tokenize='trigram'
);
CREATE VIEW repository_search_text AS
SELECT rowid, owner_id, id, json_extract(metadata,'$.full_name') AS name,
       printf('%s %s %s %s %s %s %s', json_extract(metadata,'$.description'),
           json_extract(metadata,'$.language'), json_extract(metadata,'$.topics'),
           json_extract(metadata,'$.license'), tags, note, summary) AS body
FROM repositories WHERE active=1;

INSERT INTO repository_fts(rowid, owner, repo_id, name, body)
SELECT rowid, owner_id, id, name, body FROM repository_search_text;

CREATE TRIGGER repository_fts_insert AFTER INSERT ON repositories
WHEN NEW.active=1 BEGIN
    INSERT INTO repository_fts(rowid, owner, repo_id, name, body)
        SELECT rowid, owner_id, id, name, body FROM repository_search_text WHERE rowid=NEW.rowid;
END;

CREATE TRIGGER repository_fts_delete AFTER DELETE ON repositories BEGIN
    DELETE FROM repository_fts WHERE rowid=OLD.rowid;
    DELETE FROM index_source_versions WHERE owner=OLD.owner_id AND id=OLD.id;
END;

CREATE TRIGGER repository_fts_update AFTER UPDATE OF active, metadata, tags, note, summary ON repositories
WHEN NEW.active IS NOT OLD.active OR NEW.tags IS NOT OLD.tags OR NEW.note IS NOT OLD.note
    OR NEW.summary IS NOT OLD.summary
    OR json_extract(NEW.metadata,'$.full_name') IS NOT json_extract(OLD.metadata,'$.full_name')
    OR json_extract(NEW.metadata,'$.description') IS NOT json_extract(OLD.metadata,'$.description')
    OR json_extract(NEW.metadata,'$.language') IS NOT json_extract(OLD.metadata,'$.language')
    OR json_extract(NEW.metadata,'$.topics') IS NOT json_extract(OLD.metadata,'$.topics')
    OR json_extract(NEW.metadata,'$.license') IS NOT json_extract(OLD.metadata,'$.license')
BEGIN
    DELETE FROM repository_fts WHERE rowid=OLD.rowid;
    INSERT INTO repository_fts(rowid, owner, repo_id, name, body)
        SELECT rowid, owner_id, id, name, body FROM repository_search_text WHERE rowid=NEW.rowid;
END;

CREATE TRIGGER repository_source_update AFTER UPDATE OF metadata, tags, note, summary, code_context, commit_index ON repositories
WHEN NEW.tags IS NOT OLD.tags OR NEW.note IS NOT OLD.note OR NEW.summary IS NOT OLD.summary
    OR NEW.code_context IS NOT OLD.code_context OR NEW.commit_index IS NOT OLD.commit_index
    OR json_extract(NEW.metadata,'$.full_name') IS NOT json_extract(OLD.metadata,'$.full_name')
    OR json_extract(NEW.metadata,'$.description') IS NOT json_extract(OLD.metadata,'$.description')
    OR json_extract(NEW.metadata,'$.language') IS NOT json_extract(OLD.metadata,'$.language')
    OR json_extract(NEW.metadata,'$.topics') IS NOT json_extract(OLD.metadata,'$.topics')
    OR json_extract(NEW.metadata,'$.license') IS NOT json_extract(OLD.metadata,'$.license')
    OR json_extract(NEW.metadata,'$.readme') IS NOT json_extract(OLD.metadata,'$.readme')
BEGIN
    UPDATE repositories SET source_version=OLD.source_version+1 WHERE rowid=NEW.rowid;
END;
PRAGMA user_version=4;
