use anyhow::Result;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::db;
use crate::error::{AppError, AppResult};
use crate::ids::{new_id, random_token, sha256_hex};

#[derive(Clone, Debug)]
pub struct Actor {
    pub id: String,
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
    pub revision: i64,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub actor: Actor,
    pub csrf_token: String,
    pub expires_at: i64,
    pub event_cursor: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CapabilitySet {
    pub browse: bool,
    pub edit: bool,
    pub upload: bool,
    pub view_original: bool,
    pub download: bool,
    pub archive_enabled: bool,
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .to_string())
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn password_ok(password: &str) -> bool {
    password.chars().count() >= 10
}

pub async fn initialized(pool: &SqlitePool) -> Result<bool> {
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM actors WHERE role = 'owner'")
        .fetch_one(pool)
        .await?;
    Ok(n.0 > 0)
}

pub async fn create_owner(pool: &SqlitePool, username: &str, password: &str) -> Result<Actor> {
    let hash = hash_password(password)?;
    let id = new_id();
    let now = db::now_ms();
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, status, must_change_password, revision, created_at)
         VALUES(?, ?, ?, 'owner', 'active', 0, 1, ?)",
    )
    .bind(&id)
    .bind(username)
    .bind(&hash)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(Actor {
        id,
        username: username.to_string(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    })
}

pub async fn issue_session(pool: &SqlitePool, actor: &Actor, ttl_ms: i64) -> Result<(String, Session)> {
    let raw = random_token(32);
    let token_sha = sha256_hex(raw.as_bytes());
    let csrf = random_token(16);
    let id = new_id();
    let now = db::now_ms();
    let expires = now + ttl_ms;
    sqlx::query(
        "INSERT INTO sessions(id, actor_id, token_sha256, csrf_token, expires_at, created_at)
         VALUES(?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&actor.id)
    .bind(&token_sha)
    .bind(&csrf)
    .bind(expires)
    .bind(now)
    .execute(pool)
    .await?;
    let event_cursor = current_event_cursor(pool).await?;
    Ok((
        raw,
        Session {
            id,
            actor: actor.clone(),
            csrf_token: csrf,
            expires_at: expires,
            event_cursor,
        },
    ))
}

pub async fn current_event_cursor(pool: &SqlitePool) -> Result<i64> {
    let row: (i64,) = sqlx::query_as("SELECT COALESCE(MAX(id), 0) FROM events")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

pub async fn lookup_session(pool: &SqlitePool, raw_token: &str) -> Result<Option<Session>> {
    let sha = sha256_hex(raw_token.as_bytes());
    let now = db::now_ms();
    let row = sqlx::query_as::<_, (String, String, i64, i64, String, String, String, i64)>(
        "SELECT s.id, s.csrf_token, s.expires_at, a.must_change_password, a.id, a.username, a.role, a.revision
         FROM sessions s JOIN actors a ON a.id = s.actor_id
         WHERE s.token_sha256 = ? AND s.revoked_at IS NULL AND s.expires_at > ? AND a.status = 'active'",
    )
    .bind(&sha)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    let Some((sid, csrf, exp, must, aid, username, role, rev)) = row else {
        return Ok(None);
    };
    Ok(Some(Session {
        id: sid,
        csrf_token: csrf,
        expires_at: exp,
        event_cursor: current_event_cursor(pool).await?,
        actor: Actor {
            id: aid,
            username,
            role,
            must_change_password: must == 1,
            revision: rev,
        },
    }))
}

pub async fn session_open(pool: &SqlitePool, session_id: &str) -> Result<bool> {
    let (open,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sessions s
         JOIN actors a ON a.id = s.actor_id
         WHERE s.id = ? AND s.revoked_at IS NULL AND s.expires_at > ? AND a.status = 'active'",
    )
    .bind(session_id)
    .bind(db::now_ms())
    .fetch_one(pool)
    .await?;
    Ok(open == 1)
}

pub async fn revoke_session(pool: &SqlitePool, session_id: &str) -> Result<()> {
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(db::now_ms())
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn revoke_actor_sessions(pool: &SqlitePool, actor_id: &str) -> Result<()> {
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE actor_id = ? AND revoked_at IS NULL")
        .bind(db::now_ms())
        .bind(actor_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn find_actor_by_username(pool: &SqlitePool, username: &str) -> Result<Option<(Actor, String)>> {
    let row = sqlx::query_as::<_, (String, String, String, i64, i64, String)>(
        "SELECT id, username, role, must_change_password, revision, password_hash FROM actors
         WHERE username = ? AND status = 'active'",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, username, role, must, rev, hash)| {
        (
            Actor {
                id,
                username,
                role,
                must_change_password: must == 1,
                revision: rev,
            },
            hash,
        )
    }))
}

pub async fn album_capabilities(
    pool: &SqlitePool,
    actor: Option<&Actor>,
    album_id: &str,
) -> AppResult<CapabilitySet> {
    let album = sqlx::query_as::<_, (String, i64, Option<i64>)>(
        "SELECT visibility, archive_enabled, deleted_at FROM albums WHERE id = ?",
    )
    .bind(album_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((visibility, archive_enabled, deleted_at)) = album else {
        return Err(AppError::not_found());
    };
    if deleted_at.is_some() && actor.map(|a| a.role.as_str()) != Some("owner") {
        return Err(AppError::not_found());
    }
    if let Some(actor) = actor {
        if actor.role == "owner" {
            return Ok(CapabilitySet {
                browse: true,
                edit: true,
                upload: true,
                view_original: true,
                download: true,
                archive_enabled: archive_enabled == 1,
            });
        }
        let grant = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            "SELECT browse, edit, upload, view_original, download FROM album_grants
             WHERE album_id = ? AND actor_id = ?",
        )
        .bind(album_id)
        .bind(&actor.id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
        if let Some((b, e, u, v, d)) = grant {
            return Ok(CapabilitySet {
                browse: b == 1,
                edit: e == 1,
                upload: u == 1,
                view_original: v == 1,
                download: d == 1,
                archive_enabled: archive_enabled == 1 && d == 1,
            });
        }
    }
    if visibility == "public" {
        return Ok(CapabilitySet {
            browse: true,
            edit: false,
            upload: false,
            view_original: false,
            download: false,
            archive_enabled: false,
        });
    }
    Err(AppError::not_found())
}

pub fn require_owner(actor: &Actor) -> AppResult<()> {
    if actor.role == "owner" {
        Ok(())
    } else {
        Err(AppError::forbidden())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordSync {
    Applied,
    Unchanged,
    NeedsRebind,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PasswordMark {
    revision_mac: String,
    value_mac: String,
    pair_mac: String,
    source: String,
}

pub async fn sync_configured_password(
    pool: &SqlitePool,
    keys_root: &std::path::Path,
    username: &str,
    password: Option<&str>,
    revision: Option<&str>,
    source: &str,
    rebind: bool,
) -> Result<PasswordSync> {
    let Some(password) = password.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(PasswordSync::Unchanged);
    };
    let key = deploy_key(keys_root)?;
    let key_id = sha256_hex(&key);
    let stored_key = db::kv_get(pool, "password_config_key_id").await?;
    let key_mismatch = stored_key.as_ref().is_some_and(|stored| stored != &key_id);
    if key_mismatch && !rebind {
        return Ok(PasswordSync::NeedsRebind);
    }
    let ready = initialized(pool).await?;
    if !password_ok(password) {
        if ready {
            anyhow::bail!("configured password rejected");
        }
        return Ok(PasswordSync::Unchanged);
    }
    let revision_label = revision.unwrap_or("");
    let value_mac = password_mac(&key, "value", password);
    let revision_mac = password_mac(&key, "rev", revision_label);
    let pair_mac = password_mac(&key, "pair", &format!("{revision_label}\0{password}"));
    let mut marks = if key_mismatch {
        Vec::new()
    } else {
        password_marks(pool).await?
    };
    if !rebind {
        if let Some(existing) = marks.iter().find(|mark| mark.revision_mac == revision_mac) {
            if existing.pair_mac != pair_mac {
                anyhow::bail!("configured password revision rejected");
            }
            return Ok(PasswordSync::Unchanged);
        }
        if revision_label.is_empty() && marks.iter().any(|mark| mark.value_mac == value_mac) {
            return Ok(PasswordSync::Unchanged);
        }
    }
    let hash = hash_password(password)?;
    let mut tx = db::begin_immediate(pool).await?;
    let existing = sqlx::query_as::<_, (String,)>("SELECT id FROM actors WHERE role = 'owner' LIMIT 1")
        .fetch_optional(&mut *tx)
        .await?;
    let actor_id = if let Some((id,)) = existing {
        sqlx::query(
            "UPDATE actors SET password_hash = ?, must_change_password = 0, revision = revision + 1 WHERE id = ?",
        )
        .bind(&hash)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE sessions SET revoked_at = ? WHERE actor_id = ? AND revoked_at IS NULL")
            .bind(db::now_ms())
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        id
    } else {
        let id = new_id();
        sqlx::query(
            "INSERT INTO actors(id, username, password_hash, role, status, must_change_password, revision, created_at)
             VALUES(?, ?, ?, 'owner', 'active', 0, 1, ?)",
        )
        .bind(&id)
        .bind(username)
        .bind(&hash)
        .bind(db::now_ms())
        .execute(&mut *tx)
        .await?;
        id
    };
    if rebind || key_mismatch {
        marks.clear();
    }
    marks.retain(|mark| mark.revision_mac != revision_mac);
    marks.push(PasswordMark {
        revision_mac,
        value_mac,
        pair_mac,
        source: source.to_string(),
    });
    db::kv_set(&mut tx, "password_config_key_id", &key_id).await?;
    db::kv_set(
        &mut tx,
        "password_config_marks",
        &serde_json::to_string(&marks)?,
    )
    .await?;
    sqlx::query(
        "INSERT INTO audit_log(request_id, actor_id, action, target, result, detail, occurred_at)
         VALUES('startup', ?, 'setup.password_from_config', 'actors', 'ok', ?, ?)",
    )
    .bind(&actor_id)
    .bind(source)
    .bind(db::now_ms())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(PasswordSync::Applied)
}

async fn password_marks(pool: &SqlitePool) -> Result<Vec<PasswordMark>> {
    let Some(raw) = db::kv_get(pool, "password_config_marks").await? else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn password_mac(key: &[u8], label: &str, value: &str) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(label.as_bytes());
    message.push(0);
    message.extend_from_slice(value.as_bytes());
    hex::encode(hmac_sha256(key, &message))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for index in 0..64 {
        inner_pad[index] ^= block[index];
        outer_pad[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

pub const AUTH_WINDOW_MS: i64 = 15 * 60 * 1000;
pub const LOGIN_FAILURE_LIMIT: i64 = 5;
pub const SETUP_FAILURE_LIMIT: i64 = 5;

pub async fn auth_retry_after(
    pool: &SqlitePool,
    scope: &str,
    subject: &str,
    source: &str,
    limit: i64,
) -> Result<Option<u64>> {
    let row = sqlx::query_as::<_, (i64, i64)>(
        "SELECT failures, window_started_at FROM auth_attempts
         WHERE scope = ? AND subject = ? AND source = ?",
    )
    .bind(scope)
    .bind(subject)
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let Some((failures, started)) = row else {
        return Ok(None);
    };
    let elapsed = db::now_ms().saturating_sub(started);
    if failures >= limit && elapsed < AUTH_WINDOW_MS {
        return Ok(Some(retry_seconds(elapsed)));
    }
    Ok(None)
}

pub async fn record_auth_failure(
    pool: &SqlitePool,
    scope: &str,
    subject: &str,
    source: &str,
    limit: i64,
) -> Result<Option<u64>> {
    let now = db::now_ms();
    let mut tx = db::begin_immediate(pool).await?;
    let row = sqlx::query_as::<_, (i64, i64)>(
        "SELECT failures, window_started_at FROM auth_attempts
         WHERE scope = ? AND subject = ? AND source = ?",
    )
    .bind(scope)
    .bind(subject)
    .bind(source)
    .fetch_optional(&mut *tx)
    .await?;
    let (failures, started) = match row {
        Some((failures, started)) if now.saturating_sub(started) < AUTH_WINDOW_MS => {
            (failures + 1, started)
        }
        _ => (1, now),
    };
    sqlx::query(
        "INSERT INTO auth_attempts(scope, subject, source, failures, window_started_at)
         VALUES(?, ?, ?, ?, ?)
         ON CONFLICT(scope, subject, source) DO UPDATE SET
            failures = excluded.failures,
            window_started_at = excluded.window_started_at",
    )
    .bind(scope)
    .bind(subject)
    .bind(source)
    .bind(failures)
    .bind(started)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if failures >= limit {
        Ok(Some(retry_seconds(now.saturating_sub(started))))
    } else {
        Ok(None)
    }
}

pub async fn clear_auth_failures(pool: &SqlitePool, scope: &str, subject: &str, source: &str) -> Result<()> {
    sqlx::query("DELETE FROM auth_attempts WHERE scope = ? AND subject = ? AND source = ?")
        .bind(scope)
        .bind(subject)
        .bind(source)
        .execute(pool)
        .await?;
    Ok(())
}

fn retry_seconds(elapsed_ms: i64) -> u64 {
    let remain = AUTH_WINDOW_MS.saturating_sub(elapsed_ms);
    ((remain + 999) / 1000).max(1) as u64
}

fn deploy_key(keys_root: &std::path::Path) -> Result<Vec<u8>> {
    let path = keys_root.join("deploy.key");
    if path.is_file() {
        let bytes = std::fs::read(&path)?;
        if bytes.len() < 32 {
            anyhow::bail!("deploy key unreadable");
        }
        return Ok(bytes);
    }
    std::fs::create_dir_all(keys_root)?;
    let mut bytes = vec![0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    std::fs::write(&path, &bytes)?;
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(&path)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(&path, permissions)?;
    Ok(bytes)
}
