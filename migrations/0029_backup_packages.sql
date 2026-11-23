ALTER TABLE backup_runs ADD COLUMN format_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE backup_runs ADD COLUMN schema_version INTEGER;
ALTER TABLE backup_runs ADD COLUMN replica_verified_at INTEGER;
ALTER TABLE backup_runs ADD COLUMN replica_repository TEXT;

CREATE TABLE backup_drills (
    slot TEXT PRIMARY KEY,
    backup_id TEXT NOT NULL REFERENCES backup_runs(id),
    completed_at INTEGER NOT NULL,
    report_json TEXT NOT NULL
);

CREATE TRIGGER backup_job_terminal
AFTER UPDATE OF state ON jobs
WHEN NEW.kind = 'backup' AND NEW.state IN ('failed', 'canceled') AND OLD.state <> NEW.state
BEGIN
    UPDATE storage_route SET write_barrier = 0
    WHERE id = 1 AND current_migration_id IS NULL
      AND NOT EXISTS (SELECT 1 FROM schema_app WHERE
          (key = 'source_readonly' AND value = '1') OR (key = 'maintenance_reason' AND value <> ''))
      AND EXISTS (SELECT 1 FROM backup_runs WHERE job_id = NEW.id AND manifest_digest IS NULL
          AND state IN ('queued','running'))
      AND NOT EXISTS (SELECT 1 FROM backup_runs WHERE job_id <> NEW.id AND state IN ('queued','running'));
    UPDATE backup_runs SET state = 'failed', updated_at = NEW.updated_at
    WHERE job_id = NEW.id AND state IN ('queued','running');
END;
