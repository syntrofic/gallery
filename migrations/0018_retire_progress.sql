ALTER TABLE storage_migrations ADD COLUMN retire_done INTEGER NOT NULL DEFAULT 0;
ALTER TABLE storage_migrations ADD COLUMN retire_total INTEGER NOT NULL DEFAULT 0;
ALTER TABLE storage_migrations ADD COLUMN retire_errors INTEGER NOT NULL DEFAULT 0;
