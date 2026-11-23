CREATE TABLE storage_multipart_uploads (
    upload_id TEXT PRIMARY KEY,
    backend_id TEXT NOT NULL,
    object_key TEXT NOT NULL,
    opened_at INTEGER NOT NULL
);
