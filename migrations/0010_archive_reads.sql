CREATE TABLE archive_reads (
    archive_id TEXT NOT NULL REFERENCES archive_packages(id),
    holder TEXT NOT NULL,
    until_ms INTEGER NOT NULL,
    PRIMARY KEY (archive_id, holder)
);
