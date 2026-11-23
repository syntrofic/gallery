CREATE TABLE auth_attempts (
    scope TEXT NOT NULL,
    subject TEXT NOT NULL,
    source TEXT NOT NULL,
    failures INTEGER NOT NULL,
    window_started_at INTEGER NOT NULL,
    PRIMARY KEY (scope, subject, source)
);
