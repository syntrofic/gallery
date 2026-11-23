CREATE TABLE storage_rollback_items (
    migration_id TEXT NOT NULL REFERENCES storage_migrations(id),
    blob_id TEXT NOT NULL REFERENCES media_blobs(id),
    expected_sha256 TEXT NOT NULL,
    expected_size INTEGER NOT NULL,
    state TEXT NOT NULL,
    attempt_no INTEGER NOT NULL DEFAULT 0,
    lease_token TEXT,
    lease_until INTEGER,
    PRIMARY KEY (migration_id, blob_id)
);
