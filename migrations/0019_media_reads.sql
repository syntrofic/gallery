CREATE TABLE media_reads (
    holder TEXT PRIMARY KEY,
    backend_id TEXT NOT NULL,
    routing_epoch INTEGER NOT NULL,
    blob_id TEXT NOT NULL,
    until_ms INTEGER NOT NULL
);

CREATE INDEX media_reads_backend ON media_reads(backend_id, until_ms);
