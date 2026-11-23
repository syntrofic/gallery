CREATE TABLE search_documents (
    doc_key TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    album_id TEXT,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL
);

CREATE VIRTUAL TABLE search_fts USING fts5(
    doc_key UNINDEXED,
    body,
    tokenize = 'trigram'
);
