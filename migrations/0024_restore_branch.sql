ALTER TABLE backup_runs ADD COLUMN branch_id TEXT NOT NULL DEFAULT 'main';
ALTER TABLE restore_runs ADD COLUMN parent_backup_id TEXT;
ALTER TABLE restore_runs ADD COLUMN branch_id TEXT NOT NULL DEFAULT 'main';
