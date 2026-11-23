ALTER TABLE storage_migrations ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE storage_migrations ADD COLUMN protected_until INTEGER;
