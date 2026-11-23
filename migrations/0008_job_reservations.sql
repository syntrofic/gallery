CREATE TABLE job_reservations (
    id TEXT PRIMARY KEY,
    slots INTEGER NOT NULL CHECK (slots >= 0),
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
