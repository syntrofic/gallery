CREATE TABLE job_pauses (
    kind TEXT PRIMARY KEY,
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 200),
    actor_id TEXT,
    paused_at INTEGER NOT NULL
);
