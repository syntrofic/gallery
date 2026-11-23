use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, Transaction};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use tokio::time::{sleep, Duration};

pub const JOB_HINT_CAPACITY: usize = 64;
pub const JOB_IDLE_POLL: Duration = Duration::from_secs(1);

/// In-process wakeups only. At most 64 hints are kept; further hints merge into that batch.
pub struct JobHints {
    queued: AtomicUsize,
    notify: Notify,
}

impl JobHints {
    pub fn new() -> Self {
        Self {
            queued: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    /// Returns false when the hint is merged because the queue is already full.
    pub fn push(&self) -> bool {
        let mut current = self.queued.load(Ordering::Relaxed);
        let accepted = loop {
            if current >= JOB_HINT_CAPACITY {
                break false;
            }
            match self.queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break true,
                Err(seen) => current = seen,
            }
        };
        self.notify.notify_one();
        accepted
    }

    pub fn drain(&self) -> usize {
        self.queued.swap(0, Ordering::AcqRel)
    }

    pub async fn wait_idle(&self) {
        tokio::select! {
            _ = self.notify.notified() => {}
            _ = sleep(JOB_IDLE_POLL) => {}
        }
    }
}

use crate::db;
use crate::ids::new_id;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub status_url: String,
    pub progress: Progress,
    pub failure: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    pub done: i64,
    pub total: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobPayload {
    pub kind: String,
    pub payload_version: i64,
    pub scope: serde_json::Value,
    pub target: serde_json::Value,
    pub actor_id: Option<String>,
}

pub async fn enqueue(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    dedupe_key: &str,
    payload: &JobPayload,
    actor_id: Option<&str>,
    priority: i64,
) -> Result<String> {
    enqueue_inner(tx, kind, dedupe_key, payload, actor_id, priority, false).await
}

pub async fn enqueue_protected(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    dedupe_key: &str,
    payload: &JobPayload,
    actor_id: Option<&str>,
    priority: i64,
) -> Result<String> {
    enqueue_inner(tx, kind, dedupe_key, payload, actor_id, priority, true).await
}

async fn enqueue_inner(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    dedupe_key: &str,
    payload: &JobPayload,
    actor_id: Option<&str>,
    priority: i64,
    protected: bool,
) -> Result<String> {
    let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM jobs WHERE dedupe_key = ?")
        .bind(dedupe_key)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some((id,)) = existing {
        return Ok(id);
    }
    if !protected && long_kind(kind) {
        ensure_capacity(tx, 1).await?;
    }
    let id = new_id();
    let now = db::now_ms();
    let inserted = sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, actor_id, state, priority, available_at, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(kind)
    .bind(dedupe_key)
    .bind(serde_json::to_string(payload)?)
    .bind(payload.scope.to_string())
    .bind(actor_id)
    .bind(priority)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await;
    match inserted {
        Ok(_) => Ok(id),
        Err(err) if err.to_string().contains("UNIQUE") => {
            let existing = sqlx::query_as::<_, (String,)>("SELECT id FROM jobs WHERE dedupe_key = ?")
                .bind(dedupe_key)
                .fetch_optional(&mut **tx)
                .await?;
            existing.map(|row| row.0).ok_or_else(|| anyhow::anyhow!(err))
        }
        Err(err) => Err(err.into()),
    }
}

const DEFAULT_QUEUE_CAP: i64 = 10_000;
const RESERVATION_TTL_MS: i64 = 24 * 60 * 60 * 1000;

fn long_kind(kind: &str) -> bool {
    matches!(
        kind,
        "generate_variant"
            | "caption_image"
            | "scan_plan"
            | "reconcile"
            | "rebuild_search"
            | "build_archive"
            | "convert_image"
    )
}

async fn soft_cap(tx: &mut Transaction<'_, sqlx::Sqlite>) -> Result<i64> {
    let raw = sqlx::query_as::<_, (String,)>("SELECT value FROM schema_app WHERE key = 'queue_soft_cap'")
        .fetch_optional(&mut **tx)
        .await?;
    Ok(raw.and_then(|row| row.0.parse().ok()).unwrap_or(DEFAULT_QUEUE_CAP))
}

async fn release_expired_reservations(tx: &mut Transaction<'_, sqlx::Sqlite>) -> Result<()> {
    sqlx::query("DELETE FROM job_reservations WHERE expires_at <= ?")
        .bind(db::now_ms())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn queue_pressure(tx: &mut Transaction<'_, sqlx::Sqlite>) -> Result<i64> {
    release_expired_reservations(tx).await?;
    let (jobs,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE state IN ('queued', 'running', 'retry_wait', 'cancel_requested')
         AND (? OR kind NOT IN ('caption_image', 'scan_plan', 'agent_run'))",
    )
    .bind(crate::config::AI_FEATURES_AVAILABLE)
    .fetch_one(&mut **tx)
    .await?;
    let (reserved,): (i64,) =
        sqlx::query_as("SELECT COALESCE(SUM(slots), 0) FROM job_reservations")
            .fetch_one(&mut **tx)
            .await?;
    Ok(jobs + reserved)
}

async fn ensure_capacity(tx: &mut Transaction<'_, sqlx::Sqlite>, extra: i64) -> Result<()> {
    let pressure = queue_pressure(tx).await?;
    let cap = soft_cap(tx).await?;
    if pressure + extra > cap {
        anyhow::bail!("QUEUE_FULL");
    }
    Ok(())
}

pub async fn reserve_slots(tx: &mut Transaction<'_, sqlx::Sqlite>, id: &str, slots: i64) -> Result<()> {
    if slots <= 0 {
        return Ok(());
    }
    ensure_capacity(tx, slots).await?;
    let now = db::now_ms();
    sqlx::query(
        "INSERT INTO job_reservations(id, slots, expires_at, created_at) VALUES(?, ?, ?, ?)",
    )
    .bind(id)
    .bind(slots)
    .bind(now + RESERVATION_TTL_MS)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn consume_reservation(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    id: &str,
    slots: i64,
) -> Result<()> {
    let current = sqlx::query_as::<_, (i64, i64)>(
        "SELECT slots, expires_at FROM job_reservations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((held, expires_at)) = current else {
        return Ok(());
    };
    if expires_at <= db::now_ms() {
        sqlx::query("DELETE FROM job_reservations WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    if held <= slots {
        sqlx::query("DELETE FROM job_reservations WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("UPDATE job_reservations SET slots = slots - ? WHERE id = ?")
            .bind(slots)
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub async fn release_reservation(tx: &mut Transaction<'_, sqlx::Sqlite>, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM job_reservations WHERE id = ?")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<JobView>> {
    let row = sqlx::query_as::<_, (String, String, String, i64, i64, Option<String>)>(
        "SELECT id, kind, state, progress_done, progress_total, failure_code FROM jobs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, kind, state, done, total, failure)| JobView {
        status_url: format!("/api/v1/jobs/{id}"),
        id,
        kind,
        state,
        progress: Progress { done, total },
        failure,
    }))
}

async fn emit_finished(
    pool: &SqlitePool,
    rows: Vec<(String, String, Option<String>, String, Option<String>)>,
) -> Result<()> {
    for (id, kind, actor, state, failure) in rows {
        let progress = sqlx::query_as::<_, (i64, i64)>(
            "SELECT progress_done, progress_total FROM jobs WHERE id = ?",
        )
        .bind(&id)
        .fetch_optional(pool)
        .await?
        .unwrap_or((0, 0));
        crate::events::emit(
            pool,
            "job.finished",
            "job",
            Some(&id),
            Some(&id),
            None,
            actor.as_deref(),
            job_notice(&id, &kind, &state, progress.0, progress.1, failure.as_deref()),
        )
        .await?;
    }
    Ok(())
}

pub async fn cancel(pool: &SqlitePool, id: &str) -> Result<Option<JobView>> {
    let finished = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
        "UPDATE jobs SET state = 'canceled', updated_at = ?
         WHERE id = ? AND state IN ('queued', 'retry_wait')
         RETURNING id, kind, actor_id, state, failure_code",
    )
    .bind(db::now_ms())
    .bind(id)
    .fetch_all(pool)
    .await?;
    emit_finished(pool, finished).await?;
    sqlx::query(
        "UPDATE jobs SET state = 'cancel_requested', updated_at = ?
         WHERE id = ? AND state = 'running'",
    )
    .bind(db::now_ms())
    .bind(id)
    .execute(pool)
    .await?;
    get(pool, id).await
}

pub async fn retry(pool: &SqlitePool, id: &str) -> Result<Option<JobView>> {
    sqlx::query(
        "UPDATE jobs SET state = 'queued', available_at = ?, run_generation = run_generation + 1,
            max_attempts = attempt_no + 3, failure_code = NULL,
            lease_token = NULL, lease_until = NULL, updated_at = ?
         WHERE id = ? AND state IN ('failed', 'canceled')",
    )
    .bind(db::now_ms())
    .bind(db::now_ms())
    .bind(id)
    .execute(pool)
    .await?;
    get(pool, id).await
}

struct Claimed {
    id: String,
    kind: String,
    payload: String,
    lease: String,
    attempt: i64,
}

pub async fn reclaim_expired(pool: &SqlitePool) -> Result<u64> {
    let now = db::now_ms();
    let exhausted = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
        "UPDATE jobs SET state = 'failed', failure_code = 'ATTEMPTS_EXHAUSTED', lease_token = NULL, updated_at = ?
         WHERE state IN ('queued', 'retry_wait', 'running') AND attempt_no >= max_attempts
           AND (state != 'running' OR (lease_until IS NOT NULL AND lease_until < ?))
         RETURNING id, kind, actor_id, state, failure_code",
    )
    .bind(now)
    .bind(now)
    .fetch_all(pool)
    .await?;
    emit_finished(pool, exhausted).await?;
    let res = sqlx::query(
        "UPDATE jobs SET state = 'retry_wait', available_at = ?, lease_token = NULL, updated_at = ?
         WHERE state = 'running' AND lease_until IS NOT NULL AND lease_until < ? AND attempt_no < max_attempts",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    let expired = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
        "UPDATE jobs SET state = 'failed', failure_code = 'LEASE_EXPIRED', lease_token = NULL, updated_at = ?
         WHERE state = 'running' AND lease_until IS NOT NULL AND lease_until < ? AND attempt_no >= max_attempts
         RETURNING id, kind, actor_id, state, failure_code",
    )
    .bind(now)
    .bind(now)
    .fetch_all(pool)
    .await?;
    emit_finished(pool, expired).await?;
    Ok(res.rows_affected())
}

pub async fn commit_result(
    pool: &SqlitePool,
    job_id: &str,
    lease: &str,
    ok: bool,
    failure: Option<&str>,
) -> Result<bool> {
    let now = db::now_ms();
    let state = if ok { "succeeded" } else { "failed" };
    let res = sqlx::query(
        "UPDATE jobs SET state = ?, failure_code = ?, lease_token = NULL,
            progress_done = CASE WHEN ? THEN progress_total ELSE progress_done END, updated_at = ?
         WHERE id = ? AND lease_token = ? AND state = 'running' AND lease_until > ?",
    )
    .bind(state)
    .bind(failure)
    .bind(ok)
    .bind(now)
    .bind(job_id)
    .bind(lease)
    .bind(now)
    .execute(pool)
    .await?;
    if res.rows_affected() == 1 {
        sqlx::query(
            "UPDATE job_attempts SET finished_at = ?, result = ?
             WHERE job_id = ? AND finished_at IS NULL",
        )
        .bind(now)
        .bind(state)
        .bind(job_id)
        .execute(pool)
        .await?;
        let finished = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
            "SELECT id, kind, actor_id, state, failure_code FROM jobs WHERE id = ?",
        )
        .bind(job_id)
        .fetch_all(pool)
        .await?;
        emit_finished(pool, finished).await?;
    }
    Ok(res.rows_affected() == 1)
}

pub async fn report_progress(
    pool: &SqlitePool,
    job_id: &str,
    lease: &str,
    done: i64,
    total: i64,
) -> Result<bool> {
    let now = db::now_ms();
    let mut tx = crate::db::begin_immediate(pool).await?;
    let row = sqlx::query_as::<_, (String, Option<String>, String)>(
        "UPDATE jobs SET progress_done = ?, progress_total = ?, updated_at = ?
         WHERE id = ? AND lease_token = ? AND state = 'running' AND lease_until > ?
           AND updated_at <= ?
         RETURNING kind, actor_id, state",
    )
    .bind(done)
    .bind(total)
    .bind(now)
    .bind(job_id)
    .bind(lease)
    .bind(now)
    .bind(now - 1_000)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((kind, actor, state)) = row else {
        tx.rollback().await.ok();
        return Ok(false);
    };
    crate::events::emit_tx(
        &mut tx,
        "job.progress",
        "job",
        Some(job_id),
        Some(job_id),
        None,
        actor.as_deref(),
        job_notice(job_id, &kind, &state, done, total, None),
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

fn job_notice(
    id: &str,
    kind: &str,
    state: &str,
    done: i64,
    total: i64,
    failure: Option<&str>,
) -> serde_json::Value {
    let failure = failure.filter(|code| {
        !code.contains('/') && !code.contains('\\') && !code.contains("sk-")
    });
    serde_json::json!({
        "id": id,
        "status_url": format!("/api/v1/jobs/{id}"),
        "kind": kind,
        "state": state,
        "progress": {"done": done, "total": total},
        "failure_code": failure,
    })
}

async fn renew_lease(pool: &SqlitePool, job_id: &str, lease: &str) -> Result<bool> {
    let now = db::now_ms();
    let res = sqlx::query(
        "UPDATE jobs SET lease_until = ?, updated_at = ?
         WHERE id = ? AND lease_token = ? AND state = 'running' AND lease_until > ?",
    )
    .bind(db::now_ms() + 60_000)
    .bind(db::now_ms())
    .bind(job_id)
    .bind(lease)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

const CLOCK_JUMP_MS: i64 = 30_000;

fn mono_now_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

async fn clock_text(tx: &mut Transaction<'_, sqlx::Sqlite>, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM schema_app WHERE key = ?")
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.map(|row| row.0))
}

pub async fn observe_clock(pool: &SqlitePool, wall_ms: i64, mono_ms: i64) -> Result<bool> {
    let mut tx = crate::db::begin_immediate(pool).await?;
    if clock_text(&mut tx, "clock_paused").await?.as_deref() == Some("1") {
        tx.commit().await?;
        return Ok(true);
    }
    let prev_wall = clock_text(&mut tx, "clock_wall_ms").await?;
    let prev_mono = clock_text(&mut tx, "clock_mono_ms").await?;
    let (Some(prev_wall), Some(prev_mono)) = (prev_wall, prev_mono) else {
        crate::db::kv_set(&mut tx, "clock_wall_ms", &wall_ms.to_string()).await?;
        crate::db::kv_set(&mut tx, "clock_mono_ms", &mono_ms.to_string()).await?;
        tx.commit().await?;
        return Ok(false);
    };
    let prev_wall: i64 = prev_wall.parse().unwrap_or(wall_ms);
    let prev_mono: i64 = prev_mono.parse().unwrap_or(mono_ms);
    let skew = (wall_ms - prev_wall) - (mono_ms - prev_mono);
    if skew.abs() > CLOCK_JUMP_MS {
        crate::db::kv_set(&mut tx, "clock_paused", "1").await?;
        crate::db::kv_set(&mut tx, "clock_skew", &skew.to_string()).await?;
        tx.commit().await?;
        return Ok(true);
    }
    crate::db::kv_set(&mut tx, "clock_wall_ms", &wall_ms.to_string()).await?;
    crate::db::kv_set(&mut tx, "clock_mono_ms", &mono_ms.to_string()).await?;
    tx.commit().await?;
    Ok(false)
}

pub async fn recover_clock(pool: &SqlitePool, wall_ms: i64, mono_ms: i64) -> Result<i64> {
    let mut tx = crate::db::begin_immediate(pool).await?;
    if clock_text(&mut tx, "clock_paused").await?.as_deref() != Some("1") {
        tx.commit().await?;
        return Ok(0);
    }
    let skew = clock_text(&mut tx, "clock_skew")
        .await?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    sqlx::query(
        "UPDATE jobs SET lease_until = lease_until + ?, updated_at = ?
         WHERE state = 'running' AND lease_until IS NOT NULL",
    )
    .bind(skew)
    .bind(wall_ms)
    .execute(&mut *tx)
    .await?;
    crate::db::kv_set(&mut tx, "clock_wall_ms", &wall_ms.to_string()).await?;
    crate::db::kv_set(&mut tx, "clock_mono_ms", &mono_ms.to_string()).await?;
    crate::db::kv_set(&mut tx, "clock_paused", "0").await?;
    crate::db::kv_set(&mut tx, "clock_skew", "0").await?;
    tx.commit().await?;
    Ok(skew)
}

pub async fn begin_drain(pool: &SqlitePool) -> Result<()> {
    let mut tx = crate::db::begin_immediate(pool).await?;
    crate::db::kv_set(&mut tx, "workers_draining", "1").await?;
    tx.commit().await?;
    Ok(())
}

pub async fn release_unfinished(pool: &SqlitePool) -> Result<()> {
    let now = db::now_ms();
    let canceled = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
        "UPDATE jobs SET state = 'canceled', lease_token = NULL, lease_until = NULL, updated_at = ?
         WHERE state = 'cancel_requested'
         RETURNING id, kind, actor_id, state, failure_code",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    emit_finished(pool, canceled).await?;
    sqlx::query(
        "UPDATE jobs SET lease_token = NULL, lease_until = ?, updated_at = ?
         WHERE state = 'running'",
    )
    .bind(now - 1)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn resume_workers(pool: &SqlitePool) -> Result<()> {
    let mut tx = crate::db::begin_immediate(pool).await?;
    crate::db::kv_set(&mut tx, "workers_draining", "0").await?;
    tx.commit().await?;
    Ok(())
}

pub async fn shutdown_workers(pool: &SqlitePool, grace: std::time::Duration) -> Result<()> {
    begin_drain(pool).await?;
    sleep(grace).await;
    release_unfinished(pool).await?;
    Ok(())
}

async fn claim(pool: &SqlitePool, worker: &str, ai_enabled: bool) -> Result<Option<Claimed>> {
    if crate::db::kv_get(pool, "workers_draining").await?.as_deref() == Some("1") {
        return Ok(None);
    }
    if observe_clock(pool, db::now_ms(), mono_now_ms()).await? {
        return Ok(None);
    }
    reclaim_expired(pool).await?;
    let now = db::now_ms();
    let (pending,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE state IN ('queued', 'retry_wait')
         AND (? OR kind NOT IN ('caption_image', 'scan_plan', 'agent_run'))",
    )
    .bind(ai_enabled)
    .fetch_one(pool)
    .await?;
    let suppress = pending > backlog_suppress_at(pool).await?;
    let starve_ms = starve_after_ms(pool).await?;
    let mut tx = crate::db::begin_immediate(pool).await?;
    sqlx::query(
        "UPDATE jobs SET priority = 100, updated_at = ?
         WHERE state IN ('queued', 'retry_wait') AND priority < 100 AND created_at <= ?",
    )
    .bind(now)
    .bind(now - starve_ms)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, kind, payload_json, attempt_no FROM jobs
         WHERE state IN ('queued', 'retry_wait') AND available_at <= ?
           AND attempt_no < max_attempts
           AND (? OR kind NOT IN ('caption_image', 'scan_plan', 'agent_run'))
           AND kind NOT IN (SELECT kind FROM job_pauses)
           AND (
             ? = 0
             OR (
               kind NOT IN ('scan_plan', 'caption_image', 'reconcile', 'rebuild_search')
               AND NOT (kind = 'generate_variant' AND priority < 100)
             )
           )
           AND (
             NOT EXISTS (SELECT 1 FROM storage_route WHERE id = 1 AND write_barrier = 1)
             OR (
               kind IN ('backup', 'backup_replica')
               AND EXISTS (
                 SELECT 1 FROM storage_route
                 WHERE id = 1 AND write_barrier = 1 AND current_migration_id IS NULL
               )
             )
             OR (
               kind = 'storage_migrate_copy'
               AND EXISTS (
                 SELECT 1 FROM storage_route
                 WHERE id = 1 AND write_barrier = 1 AND current_migration_id IS NOT NULL
                   AND current_migration_id = json_extract(jobs.scope_json, '$.migration_id')
               )
             )
             OR (
               kind = 'reconcile'
               AND EXISTS (
                 SELECT 1 FROM storage_route
                 WHERE id = 1 AND write_barrier = 1 AND current_migration_id IS NOT NULL
               )
             )
             OR (
               kind = 'purge_media'
               AND EXISTS (
                 SELECT 1 FROM storage_route r
                 JOIN schema_app a
                   ON a.key = 'maintenance_allow:' || r.current_migration_id || ':purge_media'
                 WHERE r.id = 1 AND r.write_barrier = 1
                   AND r.current_migration_id IS NOT NULL AND a.value = '1'
               )
             )
           )
           AND (
             SELECT COUNT(*) FROM jobs running
             WHERE running.state = 'running' AND running.kind = jobs.kind
           ) < CASE jobs.kind
             WHEN 'generate_variant' THEN 2
             WHEN 'storage_migrate_copy' THEN 2
             ELSE 1
           END
         ORDER BY priority DESC,
           (SELECT COUNT(*) FROM jobs running
            WHERE running.state = 'running' AND running.actor_id = jobs.actor_id) ASC,
           available_at ASC, created_at ASC
         LIMIT 1",
    )
    .bind(now)
    .bind(ai_enabled)
    .bind(if suppress { 1 } else { 0 })
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, kind, payload, attempt_no)) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let lease = new_id();
    let attempt = attempt_no + 1;
    sqlx::query(
        "UPDATE jobs SET state = 'running', lease_token = ?, lease_until = ?, attempt_no = ?, updated_at = ?
         WHERE id = ? AND state IN ('queued', 'retry_wait')",
    )
    .bind(&lease)
    .bind(now + 60_000)
    .bind(attempt)
    .bind(now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO job_attempts(job_id, attempt_no, worker_id, started_at) VALUES(?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(attempt)
    .bind(worker)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(Claimed {
        id,
        kind,
        payload,
        lease,
        attempt,
    }))
}

async fn complete(pool: &SqlitePool, job_id: &str, lease: &str, ok: bool, failure: Option<&str>) -> Result<()> {
    commit_result(pool, job_id, lease, ok, failure).await?;
    Ok(())
}

const RETRY_JITTER_MS: i64 = 5_000;
const RETRY_AFTER_CAP_MS: i64 = 300_000;

pub fn next_retry_delay(attempt: i64, jitter_ms: i64, retry_after_ms: Option<i64>) -> i64 {
    if let Some(after) = retry_after_ms {
        return after.clamp(0, RETRY_AFTER_CAP_MS);
    }
    let base = if attempt <= 1 { 30_000 } else { 120_000 };
    base + jitter_ms.clamp(0, RETRY_JITTER_MS)
}

fn jitter_ms(job_id: &str) -> i64 {
    let mut hash = 0u32;
    for byte in job_id.as_bytes() {
        hash = hash.wrapping_mul(16777619) ^ u32::from(*byte);
    }
    i64::from(hash % (RETRY_JITTER_MS as u32 + 1))
}

fn retry_after_ms(message: &str) -> Option<i64> {
    let lower = message.to_ascii_lowercase();
    let key = "retry-after:";
    let rest = lower.split(key).nth(1)?;
    let digits: String = rest.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    let seconds: i64 = digits.parse().ok()?;
    Some(seconds.saturating_mul(1_000))
}

async fn retry_wait(pool: &SqlitePool, job_id: &str, lease: &str, attempt: i64, message: &str) -> Result<()> {
    let delay = next_retry_delay(attempt, jitter_ms(job_id), retry_after_ms(message));
    let now = db::now_ms();
    sqlx::query(
        "UPDATE jobs SET state = 'retry_wait', available_at = ?, lease_token = NULL, updated_at = ?
         WHERE id = ? AND lease_token = ? AND attempt_no < max_attempts",
    )
    .bind(now + delay)
    .bind(now)
    .bind(job_id)
    .bind(lease)
    .execute(pool)
    .await?;
    let failed = sqlx::query_as::<_, (String, String, Option<String>, String, Option<String>)>(
        "UPDATE jobs SET state = 'failed', failure_code = COALESCE(failure_code, 'JOB_FAILED'), lease_token = NULL, updated_at = ?
         WHERE id = ? AND lease_token = ? AND attempt_no >= max_attempts
         RETURNING id, kind, actor_id, state, failure_code",
    )
    .bind(now)
    .bind(job_id)
    .bind(lease)
    .fetch_all(pool)
    .await?;
    emit_finished(pool, failed).await?;
    Ok(())
}

fn next_utc_midnight_ms() -> i64 {
    let tomorrow = chrono::Utc::now().date_naive() + chrono::Duration::days(1);
    tomorrow
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn utc_day_start_ms() -> i64 {
    chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

async fn ai_over_budget(pool: &SqlitePool) -> Result<bool> {
    let configured = crate::db::kv_get(pool, "ai_daily_token_budget").await?;
    let budget = configured
        .and_then(|value| value.parse::<i64>().ok())
        .or_else(|| {
            std::env::var("GALLERY_AI_DAILY_TOKENS")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(1_000_000);
    let (used,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(
            COALESCE(json_extract(payload_json, '$.prompt_tokens'), 0)
            + COALESCE(json_extract(payload_json, '$.completion_tokens'), 0)
         ), 0)
         FROM events
         WHERE type = 'caption.model_written' AND occurred_at >= ?",
    )
    .bind(utc_day_start_ms())
    .fetch_one(pool)
    .await?;
    Ok(used >= budget)
}

async fn defer_ai_budget(pool: &SqlitePool, job_id: &str, lease: &str) -> Result<()> {
    let now = db::now_ms();
    sqlx::query(
        "UPDATE jobs
         SET state = 'retry_wait', available_at = ?, failure_code = 'AI_BUDGET',
             attempt_no = CASE WHEN attempt_no > 0 THEN attempt_no - 1 ELSE 0 END,
             lease_token = NULL, lease_until = NULL, updated_at = ?
         WHERE id = ? AND lease_token = ?",
    )
    .bind(next_utc_midnight_ms())
    .bind(now)
    .bind(job_id)
    .bind(lease)
    .execute(pool)
    .await?;
    Ok(())
}

pub fn kind_deadline(kind: &str) -> Duration {
    match kind {
        "scan_plan" | "reconcile" => Duration::from_secs(30),
        "caption_image" => Duration::from_secs(2 * 60),
        "build_archive" | "storage_migrate_copy" | "storage_retire" => Duration::from_secs(30 * 60),
        "agent_run" => Duration::from_secs(10 * 60),
        "backup" | "restore" | "restore_drill" | "backup_replica" => Duration::from_secs(6 * 60 * 60),
        _ => Duration::from_secs(5 * 60),
    }
}

pub async fn finish_within<T>(
    budget: Duration,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(budget, work).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("permanent:DEADLINE")),
    }
}

pub fn spawn_workers(state: crate::app::AppState) {
    for _ in 0..2 {
        let state = state.clone();
        let worker_id = new_id();
        tokio::spawn(async move {
            worker_loop(state, worker_id).await;
        });
    }
}

async fn worker_loop(state: crate::app::AppState, worker_id: String) {
        loop {
            match claim(&state.pool, &worker_id, state.config.ai_enabled).await {
                Ok(Some(job)) => {
                    let pool = state.pool.clone();
                    let beat_id = job.id.clone();
                    let beat_lease = job.lease.clone();
                    let beat = tokio::spawn(async move {
                        loop {
                            sleep(Duration::from_secs(15)).await;
                            if renew_lease(&pool, &beat_id, &beat_lease).await.unwrap_or(false) == false {
                                break;
                            }
                        }
                    });
                    if matches!(job.kind.as_str(), "caption_image" | "scan_plan")
                        && ai_over_budget(&state.pool).await.unwrap_or(false)
                    {
                        beat.abort();
                        let _ = defer_ai_budget(&state.pool, &job.id, &job.lease).await;
                        continue;
                    }
                    let result = finish_within(kind_deadline(&job.kind), crate::workers::execute(
                        &state,
                        &job.kind,
                        &job.payload,
                        &job.id,
                        &job.lease,
                    ))
                    .await;
                    beat.abort();
                    match result {
                        Ok(()) => {
                            let _ = complete(&state.pool, &job.id, &job.lease, true, None).await;
                        }
                        Err(err) => {
                            let message = err.to_string();
                            tracing::warn!(error = %message, job = %job.id, "job failed");
                            if let Some(code) = message.strip_prefix("permanent:") {
                                let code = code.split_whitespace().next().unwrap_or("FAILED");
                                let _ = complete(&state.pool, &job.id, &job.lease, false, Some(code)).await;
                            } else if job.attempt >= 3 {
                                let _ = complete(
                                    &state.pool,
                                    &job.id,
                                    &job.lease,
                                    false,
                                    Some("JOB_FAILED"),
                                )
                                .await;
                            } else {
                                let _ = retry_wait(&state.pool, &job.id, &job.lease, job.attempt, &message).await;
                            }
                        }
                    }
                }
                Ok(None) => {
                    if crate::db::kv_get(&state.pool, "clock_paused").await.ok().flatten().as_deref()
                        == Some("1")
                    {
                        let _ = recover_clock(&state.pool, db::now_ms(), mono_now_ms()).await;
                    }
                    if state.job_hints.drain() > 0 {
                        continue;
                    }
                    state.job_hints.wait_idle().await;
                }
                Err(err) => tracing::error!(error = %err, "claim job"),
            }
        }
}

const DEFAULT_BACKLOG_SUPPRESS: i64 = 1_000;
const DEFAULT_STARVE_MS: i64 = 3_600_000;

async fn starve_after_ms(pool: &SqlitePool) -> Result<i64> {
    let raw = crate::db::kv_get(pool, "queue_starve_ms").await?;
    Ok(raw
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_STARVE_MS))
}

async fn backlog_suppress_at(pool: &SqlitePool) -> Result<i64> {
    let raw = crate::db::kv_get(pool, "queue_backlog_suppress").await?;
    Ok(raw.and_then(|value| value.parse().ok()).unwrap_or(DEFAULT_BACKLOG_SUPPRESS))
}

pub async fn claim_next(pool: &SqlitePool, worker: &str) -> Result<Option<(String, String)>> {
    Ok(claim(pool, worker, true).await?.map(|job| (job.id, job.kind)))
}

pub async fn pause_kind(pool: &SqlitePool, actor_id: &str, kind: &str, reason: &str) -> Result<()> {
    let reason = reason.trim();
    if kind.is_empty() || !known_kind(kind) || reason.is_empty() || reason.chars().count() > 200 {
        anyhow::bail!("invalid pause");
    }
    sqlx::query(
        "INSERT INTO job_pauses(kind, reason, actor_id, paused_at) VALUES(?, ?, ?, ?)
         ON CONFLICT(kind) DO UPDATE SET reason = excluded.reason, actor_id = excluded.actor_id, paused_at = excluded.paused_at",
    )
    .bind(kind)
    .bind(reason)
    .bind(actor_id)
    .bind(db::now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn resume_kind(pool: &SqlitePool, kind: &str) -> Result<bool> {
    let removed = sqlx::query("DELETE FROM job_pauses WHERE kind = ?")
        .bind(kind)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() == 1)
}

pub async fn summary(pool: &SqlitePool) -> Result<serde_json::Value> {
    let counts = sqlx::query_as::<_, (String, i64)>(
        "SELECT state, COUNT(*) FROM jobs
         WHERE state IN ('queued', 'running', 'retry_wait', 'failed')
         GROUP BY state",
    )
    .fetch_all(pool)
    .await?;
    let mut backlog = serde_json::json!({"queued": 0, "running": 0, "retry_wait": 0, "failed": 0});
    for (state, count) in counts {
        backlog[state] = serde_json::json!(count);
    }
    let pauses = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT kind, reason, paused_at FROM job_pauses ORDER BY kind",
    )
    .fetch_all(pool)
    .await?;
    let failures = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, kind, failure_code FROM jobs WHERE state = 'failed' ORDER BY updated_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await?;
    let now = db::now_ms();
    let oldest = sqlx::query_as::<_, (Option<i64>,)>(
        "SELECT MIN(created_at) FROM jobs WHERE state IN ('queued', 'retry_wait')",
    )
    .fetch_one(pool)
    .await?
    .0;
    let oldest_pending_seconds = oldest
        .map(|created| ((now - created) / 1000).max(0))
        .unwrap_or(0);
    let (expired_leases_total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE state = 'running' AND lease_until IS NOT NULL AND lease_until < ?",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    let (pending,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE state IN ('queued', 'retry_wait')",
    )
    .fetch_one(pool)
    .await?;
    let (ready_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs
         WHERE state IN ('queued', 'retry_wait') AND available_at <= ?",
    )
    .bind(now)
    .fetch_one(pool)
    .await?;
    let (failed, terminal): (i64, i64) = sqlx::query_as(
        "SELECT
            COALESCE(SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN state IN ('failed', 'succeeded') THEN 1 ELSE 0 END), 0)
         FROM jobs",
    )
    .fetch_one(pool)
    .await?;
    let failure_rate = if terminal == 0 { 0.0 } else { failed as f64 / terminal as f64 };
    let (retries,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM job_attempts WHERE attempt_no > 1",
    )
    .fetch_one(pool)
    .await?;
    let (execution_ms,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(finished_at - started_at), 0) FROM job_attempts
         WHERE finished_at IS NOT NULL AND finished_at >= started_at",
    )
    .fetch_one(pool)
    .await?;
    Ok(serde_json::json!({
        "backlog": backlog,
        "ready_count": ready_count,
        "oldest_pending_seconds": oldest_pending_seconds,
        "expired_leases_total": expired_leases_total,
        "failure_rate": failure_rate,
        "retries": retries,
        "execution_ms": execution_ms,
        "suppressed": pending > backlog_suppress_at(pool).await?,
        "pauses": pauses.into_iter().map(|(kind, reason, paused_at)| serde_json::json!({
            "kind": kind,
            "reason": reason,
            "paused_at": db::rfc3339(paused_at)
        })).collect::<Vec<_>>(),
        "recent_failures": failures.into_iter().map(|(id, kind, code)| serde_json::json!({
            "id": id,
            "kind": kind,
            "failure_code": code
        })).collect::<Vec<_>>()
    }))
}

fn known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "generate_variant"
            | "build_archive"
            | "purge_media"
            | "backup"
            | "backup_replica"
            | "restore_drill"
            | "restore"
            | "storage_migrate_copy"
            | "caption_image"
            | "scan_plan"
            | "agent_run"
            | "convert_image"
            | "reconcile"
            | "rebuild_search"
    )
}

pub async fn list(
    pool: &SqlitePool,
    actor_id: Option<&str>,
    owner: bool,
    limit: i64,
) -> Result<Vec<JobView>> {
    let rows = if owner {
        sqlx::query_as::<_, (String, String, String, i64, i64, Option<String>)>(
            "SELECT id, kind, state, progress_done, progress_total, failure_code FROM jobs
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else if let Some(actor_id) = actor_id {
        sqlx::query_as(
            "SELECT id, kind, state, progress_done, progress_total, failure_code FROM jobs
             WHERE actor_id = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(actor_id)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        vec![]
    };
    Ok(rows
        .into_iter()
        .map(|(id, kind, state, done, total, failure)| JobView {
            status_url: format!("/api/v1/jobs/{id}"),
            id,
            kind,
            state,
            progress: Progress { done, total },
            failure,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_ai_leaves_existing_work_unclaimed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = crate::db::connect(&dir.path().join("gallery.sqlite")).await?;
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
             VALUES('caption', 'caption_image', 'caption-test', '{}', '{}', 'queued', ?, ?, ?)",
        )
        .bind(db::now_ms())
        .bind(db::now_ms())
        .bind(db::now_ms())
        .execute(&pool)
        .await?;
        assert!(claim(&pool, "worker-with-ai-disabled", false).await?.is_none());
        let mut tx = crate::db::begin_immediate(&pool).await?;
        assert_eq!(queue_pressure(&mut tx).await?, 0);
        tx.commit().await?;
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = 'caption'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(state, "queued");
        assert!(claim(&pool, "worker-with-ai-enabled", true).await?.is_some());
        Ok(())
    }

    #[test]
    fn hint_queue_merges_once_it_holds_sixty_four() {
        let hints = JobHints::new();
        let mut accepted = 0;
        let mut merged = 0;
        for _ in 0..70 {
            if hints.push() {
                accepted += 1;
            } else {
                merged += 1;
            }
        }
        assert_eq!(accepted, JOB_HINT_CAPACITY);
        assert_eq!(merged, 6);
        assert_eq!(hints.drain(), JOB_HINT_CAPACITY);
        assert_eq!(hints.drain(), 0);
        assert!(hints.push());
    }

    #[tokio::test]
    async fn hint_wakes_an_idle_waiter_without_the_poll() {
        let hints = std::sync::Arc::new(JobHints::new());
        let waiting = hints.clone();
        let started = std::time::Instant::now();
        let handle = tokio::spawn(async move {
            waiting.wait_idle().await;
        });
        tokio::task::yield_now().await;
        assert!(hints.push());
        handle.await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn idle_poll_is_one_second_without_a_hint() {
        assert_eq!(JOB_IDLE_POLL, Duration::from_secs(1));
        let hints = JobHints::new();
        let started = std::time::Instant::now();
        hints.wait_idle().await;
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn kind_deadlines_follow_the_queue_table() {
        assert_eq!(kind_deadline("generate_variant"), Duration::from_secs(5 * 60));
        assert_eq!(kind_deadline("convert_image"), Duration::from_secs(5 * 60));
        assert_eq!(kind_deadline("purge_media"), Duration::from_secs(5 * 60));
        assert_eq!(kind_deadline("caption_image"), Duration::from_secs(2 * 60));
        assert_eq!(kind_deadline("build_archive"), Duration::from_secs(30 * 60));
        assert_eq!(kind_deadline("scan_plan"), Duration::from_secs(30));
        assert_eq!(kind_deadline("reconcile"), Duration::from_secs(30));
        assert_eq!(kind_deadline("agent_run"), Duration::from_secs(10 * 60));
        assert_eq!(kind_deadline("backup"), Duration::from_secs(6 * 60 * 60));
    }

    #[tokio::test]
    async fn work_past_its_deadline_fails_without_waiting_for_the_lease() {
        let started = std::time::Instant::now();
        let err = finish_within(Duration::from_millis(40), async {
            sleep(Duration::from_secs(30)).await;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap_err();
        assert!(err.to_string().starts_with("permanent:DEADLINE"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }
}
