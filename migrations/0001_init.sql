CREATE TABLE schema_app (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE actors (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('owner', 'collaborator')),
    status TEXT NOT NULL CHECK (status IN ('active', 'disabled')) DEFAULT 'active',
    must_change_password INTEGER NOT NULL CHECK (must_change_password IN (0, 1)) DEFAULT 0,
    revision INTEGER NOT NULL CHECK (revision >= 1) DEFAULT 1,
    created_at INTEGER NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    actor_id TEXT NOT NULL REFERENCES actors(id),
    token_sha256 TEXT NOT NULL UNIQUE,
    csrf_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE admin_password_source_marks (
    source TEXT NOT NULL,
    value_digest TEXT NOT NULL,
    revision_digest TEXT NOT NULL,
    applied_at INTEGER NOT NULL,
    PRIMARY KEY (source, revision_digest)
);

CREATE TABLE bootstrap_tokens (
    token_sha256 TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER
);

CREATE TABLE media_blobs (
    id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL CHECK (size >= 0),
    mime TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('staging', 'verified', 'quarantined', 'deleted')),
    kind TEXT NOT NULL CHECK (kind IN ('original', 'variant', 'archive', 'inference', 'other')) DEFAULT 'original',
    created_at INTEGER NOT NULL
);

CREATE TABLE storage_backends (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('local', 's3', 'webdav')),
    config_json TEXT NOT NULL,
    config_revision INTEGER NOT NULL CHECK (config_revision >= 1) DEFAULT 1,
    capabilities_json TEXT,
    created_at INTEGER NOT NULL
);

CREATE TABLE media_locations (
    blob_id TEXT NOT NULL REFERENCES media_blobs(id),
    backend_id TEXT NOT NULL REFERENCES storage_backends(id),
    object_key TEXT NOT NULL,
    backend_revision INTEGER NOT NULL DEFAULT 1,
    size INTEGER NOT NULL CHECK (size >= 0),
    sha256 TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'verified', 'failed')),
    verified_at INTEGER,
    PRIMARY KEY (blob_id, backend_id),
    UNIQUE (backend_id, object_key)
);

CREATE TABLE storage_route (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    active_backend_id TEXT NOT NULL REFERENCES storage_backends(id),
    routing_epoch INTEGER NOT NULL DEFAULT 1,
    catalog_revision INTEGER NOT NULL DEFAULT 1,
    write_barrier INTEGER NOT NULL CHECK (write_barrier IN (0, 1)) DEFAULT 0,
    current_migration_id TEXT
);

CREATE TABLE storage_migrations (
    id TEXT PRIMARY KEY,
    source_backend_id TEXT NOT NULL REFERENCES storage_backends(id),
    target_backend_id TEXT NOT NULL REFERENCES storage_backends(id),
    direction TEXT NOT NULL,
    stage TEXT NOT NULL,
    snapshot_catalog_revision INTEGER,
    seal_json TEXT,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE storage_migration_items (
    migration_id TEXT NOT NULL REFERENCES storage_migrations(id),
    blob_id TEXT NOT NULL REFERENCES media_blobs(id),
    source_key TEXT,
    target_key TEXT,
    expected_sha256 TEXT NOT NULL,
    expected_size INTEGER NOT NULL,
    state TEXT NOT NULL,
    attempt_no INTEGER NOT NULL DEFAULT 0,
    lease_token TEXT,
    lease_until INTEGER,
    last_error TEXT,
    verified_at INTEGER,
    PRIMARY KEY (migration_id, blob_id)
);

CREATE TABLE images (
    id TEXT PRIMARY KEY,
    source_sha256 TEXT NOT NULL UNIQUE,
    original_blob_id TEXT NOT NULL UNIQUE REFERENCES media_blobs(id),
    original_name TEXT NOT NULL,
    width INTEGER NOT NULL CHECK (width > 0),
    height INTEGER NOT NULL CHECK (height > 0),
    orientation INTEGER NOT NULL DEFAULT 1,
    original_mime TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('active', 'trashed', 'purging', 'purged')) DEFAULT 'active',
    revision INTEGER NOT NULL CHECK (revision >= 1) DEFAULT 1,
    lifecycle_revision INTEGER NOT NULL CHECK (lifecycle_revision >= 1) DEFAULT 1,
    created_seq INTEGER NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    deleted_at INTEGER
);

CREATE TABLE original_reservations (
    source_sha256 TEXT PRIMARY KEY,
    image_id TEXT NOT NULL UNIQUE,
    lease_token TEXT NOT NULL,
    lease_until INTEGER NOT NULL,
    planned_object_key TEXT NOT NULL
);

CREATE TABLE albums (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
    description TEXT NOT NULL DEFAULT '' CHECK (length(description) <= 4000),
    cover_image_id TEXT REFERENCES images(id) ON DELETE SET NULL,
    visibility TEXT NOT NULL CHECK (visibility IN ('private', 'public')) DEFAULT 'private',
    archive_enabled INTEGER NOT NULL CHECK (archive_enabled IN (0, 1)) DEFAULT 1,
    revision INTEGER NOT NULL CHECK (revision >= 1) DEFAULT 1,
    content_revision INTEGER NOT NULL CHECK (content_revision >= 1) DEFAULT 1,
    acl_revision INTEGER NOT NULL CHECK (acl_revision >= 1) DEFAULT 1,
    deleted_at INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE album_images (
    album_id TEXT NOT NULL REFERENCES albums(id),
    image_id TEXT NOT NULL REFERENCES images(id),
    sort_key INTEGER NOT NULL,
    added_at INTEGER NOT NULL,
    display_name TEXT,
    PRIMARY KEY (album_id, image_id)
);
CREATE INDEX album_images_sort ON album_images (album_id, sort_key, image_id);
CREATE INDEX album_images_by_image ON album_images (image_id, album_id);

CREATE TABLE album_grants (
    album_id TEXT NOT NULL REFERENCES albums(id),
    actor_id TEXT NOT NULL REFERENCES actors(id),
    browse INTEGER NOT NULL CHECK (browse IN (0, 1)) DEFAULT 1,
    edit INTEGER NOT NULL CHECK (edit IN (0, 1)) DEFAULT 0,
    upload INTEGER NOT NULL CHECK (upload IN (0, 1)) DEFAULT 0,
    view_original INTEGER NOT NULL CHECK (view_original IN (0, 1)) DEFAULT 0,
    download INTEGER NOT NULL CHECK (download IN (0, 1)) DEFAULT 0,
    PRIMARY KEY (album_id, actor_id)
);

CREATE TABLE variant_profiles (
    profile_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    specs_json TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (profile_id, revision)
);

CREATE TABLE image_variants (
    id TEXT PRIMARY KEY,
    image_id TEXT NOT NULL REFERENCES images(id),
    profile_revision INTEGER NOT NULL,
    spec_id TEXT NOT NULL,
    blob_id TEXT REFERENCES media_blobs(id),
    state TEXT NOT NULL CHECK (state IN ('pending', 'processing', 'published', 'failed', 'superseded')),
    width INTEGER,
    height INTEGER,
    failure_code TEXT,
    UNIQUE (image_id, profile_revision, spec_id)
);

CREATE TABLE variant_heads (
    image_id TEXT NOT NULL REFERENCES images(id),
    spec_id TEXT NOT NULL,
    variant_id TEXT NOT NULL REFERENCES image_variants(id),
    published_at INTEGER NOT NULL,
    PRIMARY KEY (image_id, spec_id)
);

CREATE TABLE upload_batches (
    id TEXT PRIMARY KEY,
    album_id TEXT NOT NULL REFERENCES albums(id),
    actor_id TEXT NOT NULL REFERENCES actors(id),
    state TEXT NOT NULL CHECK (state IN ('waiting', 'running', 'succeeded', 'partial', 'failed', 'canceled')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE upload_items (
    id TEXT PRIMARY KEY,
    batch_id TEXT NOT NULL REFERENCES upload_batches(id),
    client_item_id TEXT NOT NULL,
    original_name TEXT NOT NULL,
    declared_size INTEGER NOT NULL CHECK (declared_size >= 0),
    request_fingerprint TEXT,
    state TEXT NOT NULL CHECK (state IN ('waiting', 'receiving', 'validating', 'committed', 'failed', 'canceled')),
    attempt_token TEXT,
    staged_key TEXT,
    computed_sha TEXT,
    result_image_id TEXT REFERENCES images(id),
    failure_code TEXT,
    UNIQUE (batch_id, client_item_id)
);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    dedupe_key TEXT NOT NULL UNIQUE,
    run_generation INTEGER NOT NULL DEFAULT 1,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    payload_version INTEGER NOT NULL DEFAULT 1,
    payload_json TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    actor_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'retry_wait', 'cancel_requested', 'succeeded', 'failed', 'canceled')),
    priority INTEGER NOT NULL DEFAULT 0,
    available_at INTEGER NOT NULL,
    lease_token TEXT,
    lease_until INTEGER,
    attempt_no INTEGER NOT NULL DEFAULT 0,
    progress_total INTEGER NOT NULL DEFAULT 1,
    progress_done INTEGER NOT NULL DEFAULT 0,
    failure_code TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX jobs_claim ON jobs (state, available_at, priority, created_at);

CREATE TABLE job_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id TEXT NOT NULL REFERENCES jobs(id),
    attempt_no INTEGER NOT NULL,
    worker_id TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    result TEXT,
    UNIQUE (job_id, attempt_no)
);

CREATE TABLE archive_packages (
    id TEXT PRIMARY KEY,
    album_id TEXT NOT NULL REFERENCES albums(id),
    actor_id TEXT NOT NULL REFERENCES actors(id),
    content_revision INTEGER NOT NULL,
    acl_revision INTEGER NOT NULL,
    policy_epoch INTEGER NOT NULL DEFAULT 1,
    archive_state TEXT NOT NULL CHECK (archive_state IN ('queued', 'building', 'ready', 'failed', 'expired', 'stale', 'canceled')),
    blob_id TEXT REFERENCES media_blobs(id),
    job_id TEXT REFERENCES jobs(id),
    expires_at INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE archive_items (
    archive_id TEXT NOT NULL REFERENCES archive_packages(id),
    position INTEGER NOT NULL,
    image_id TEXT NOT NULL REFERENCES images(id),
    blob_id TEXT NOT NULL REFERENCES media_blobs(id),
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (archive_id, position)
);

CREATE TABLE download_claims (
    id TEXT PRIMARY KEY,
    actor_id TEXT NOT NULL REFERENCES actors(id),
    album_id TEXT NOT NULL REFERENCES albums(id),
    image_id TEXT NOT NULL REFERENCES images(id),
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE conversions (
    id TEXT PRIMARY KEY,
    actor_id TEXT NOT NULL REFERENCES actors(id),
    job_id TEXT REFERENCES jobs(id),
    source_image_id TEXT NOT NULL REFERENCES images(id),
    spec_json TEXT NOT NULL,
    state TEXT NOT NULL,
    output_blob_id TEXT REFERENCES media_blobs(id),
    expires_at INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE captions (
    image_id TEXT NOT NULL REFERENCES images(id),
    source TEXT NOT NULL CHECK (source IN ('human', 'model')),
    model_key TEXT,
    prompt_revision TEXT,
    input_sha TEXT,
    text TEXT NOT NULL,
    review_state TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (image_id, source)
);

CREATE TABLE ai_scans (
    id TEXT PRIMARY KEY,
    actor_id TEXT NOT NULL REFERENCES actors(id),
    scope_json TEXT NOT NULL,
    state TEXT NOT NULL,
    total INTEGER NOT NULL DEFAULT 0,
    cursor_seq INTEGER,
    job_id TEXT REFERENCES jobs(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE agent_sessions (
    id TEXT PRIMARY KEY,
    actor_id TEXT NOT NULL REFERENCES actors(id),
    scope_json TEXT NOT NULL,
    upstream_thread_id TEXT,
    state TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE agent_turns (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES agent_sessions(id),
    upstream_turn_id TEXT,
    generation INTEGER NOT NULL DEFAULT 1,
    state TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL
);

CREATE TABLE agent_messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES agent_sessions(id),
    seq INTEGER NOT NULL,
    client_message_id TEXT NOT NULL,
    mode TEXT NOT NULL,
    delivery_state TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (session_id, client_message_id),
    UNIQUE (session_id, seq)
);

CREATE TABLE agent_actions (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES agent_sessions(id),
    turn_id TEXT REFERENCES agent_turns(id),
    plan_revision INTEGER NOT NULL DEFAULT 1,
    result_ref TEXT,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE agent_turn_jobs (
    turn_id TEXT NOT NULL REFERENCES agent_turns(id),
    action_id TEXT,
    job_id TEXT NOT NULL REFERENCES jobs(id),
    submitted_at INTEGER NOT NULL,
    PRIMARY KEY (turn_id, job_id)
);

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    type TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT,
    resource_id TEXT,
    revision INTEGER,
    actor_id TEXT,
    payload_json TEXT NOT NULL DEFAULT '{}',
    occurred_at INTEGER NOT NULL
);
CREATE INDEX events_scope ON events (scope_kind, scope_id, id);

CREATE TABLE idempotency_records (
    actor_id TEXT NOT NULL,
    method TEXT NOT NULL,
    route TEXT NOT NULL,
    key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    status INTEGER NOT NULL,
    response_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (actor_id, method, route, key)
);

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT,
    actor_id TEXT,
    delegated_by TEXT,
    agent_session_id TEXT,
    agent_turn_id TEXT,
    action_id TEXT,
    job_id TEXT,
    action TEXT NOT NULL,
    target TEXT,
    result TEXT NOT NULL,
    detail TEXT,
    occurred_at INTEGER NOT NULL
);

CREATE TABLE backup_runs (
    id TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    snapshot_at INTEGER,
    routing_epoch INTEGER,
    catalog_revision INTEGER,
    manifest_digest TEXT,
    job_id TEXT REFERENCES jobs(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE backup_items (
    backup_id TEXT NOT NULL REFERENCES backup_runs(id),
    blob_id TEXT NOT NULL,
    ciphertext_key TEXT,
    plaintext_sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY (backup_id, blob_id)
);

CREATE TABLE backup_pins (
    backup_id TEXT NOT NULL REFERENCES backup_runs(id),
    blob_id TEXT NOT NULL REFERENCES media_blobs(id),
    PRIMARY KEY (backup_id, blob_id)
);

CREATE TABLE restore_runs (
    id TEXT PRIMARY KEY,
    backup_id TEXT NOT NULL,
    state TEXT NOT NULL,
    job_id TEXT REFERENCES jobs(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE restore_items (
    restore_id TEXT NOT NULL REFERENCES restore_runs(id),
    blob_id TEXT NOT NULL,
    state TEXT NOT NULL,
    PRIMARY KEY (restore_id, blob_id)
);

CREATE TABLE maintenance_runs (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    resource_id TEXT,
    state TEXT NOT NULL,
    progress_json TEXT,
    detail_url TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE schema_migrations_app (
    id TEXT PRIMARY KEY,
    before_schema TEXT,
    after_schema TEXT,
    checksum TEXT NOT NULL,
    status TEXT NOT NULL,
    applied_at INTEGER NOT NULL
);
