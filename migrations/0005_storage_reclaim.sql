CREATE TABLE storage_reclaim (
    backend_id TEXT NOT NULL,
    object_key TEXT NOT NULL,
    marked_at INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('marked', 'deleted')),
    PRIMARY KEY (backend_id, object_key)
);
