pub mod local;
pub mod reconcile;
pub mod s3;
pub mod webdav;

pub use local::LocalFs;
pub use s3::S3Backend;
pub use webdav::WebDavBackend;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use std::pin::Pin;
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
#[error("storage object length or checksum mismatch")]
pub struct ObjectMismatch;

pub type ObjectStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes>> + Send>>;
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, Transaction};
use std::path::PathBuf;

use crate::ids::{image_dir_prefix, new_id, sha256_hex};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct BackendCaps {
    pub range: bool,
    pub conditional_put: bool,
    pub checksum: bool,
    pub move_object: bool,
}

#[derive(Clone, Debug)]
pub struct ObjectMeta {
    pub size: u64,
    pub sha256: String,
    pub mime: String,
}

#[derive(Clone, Debug)]
pub struct StagedObject {
    pub key: String,
    pub size: u64,
    pub sha256: String,
}

#[async_trait]
pub trait MediaBackend: Send + Sync {
    fn kind(&self) -> &'static str;
    async fn probe(&self) -> Result<BackendCaps>;
    async fn stage(&self, key: &str, bytes: &[u8]) -> Result<StagedObject>;
    async fn put_immutable(&self, key: &str, bytes: &[u8], mime: &str) -> Result<ObjectMeta>;
    async fn put_file(&self, key: &str, path: &std::path::Path, mime: &str) -> Result<ObjectMeta> {
        let bytes = std::fs::read(path)?;
        self.put_immutable(key, &bytes, mime).await
    }
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>>;
    async fn open(&self, key: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>>;
    async fn stream(&self, key: &str) -> Result<ObjectStream>;
    fn local_file(&self, _key: &str) -> Option<std::path::PathBuf> {
        None
    }
    async fn delete(&self, key: &str) -> Result<()>;
    fn bind_catalog(&self, _pool: SqlitePool, _backend_id: String) {}
    async fn abort_multipart(&self, _key: &str, _upload_id: &str) -> Result<()> {
        Ok(())
    }
    async fn abort_stale_multiparts(&self, _cutoff_ms: i64) -> Result<u64> {
        Ok(0)
    }
    async fn walk_owned(&self, _after: Option<&str>, _limit: i64) -> Result<Vec<String>> {
        anyhow::bail!("WALK_UNAVAILABLE");
    }
}

#[derive(Clone)]
pub struct Storage {
    pub backend_id: String,
    pub kind: String,
    inner: std::sync::Arc<dyn MediaBackend>,
}

impl Storage {
    pub fn new(backend_id: String, inner: std::sync::Arc<dyn MediaBackend>) -> Self {
        Self {
            kind: inner.kind().to_string(),
            backend_id,
            inner,
        }
    }

    pub fn backend(&self) -> &dyn MediaBackend {
        &*self.inner
    }

    pub async fn put_bytes(&self, key: &str, bytes: &[u8], mime: &str) -> Result<ObjectMeta> {
        self.inner.put_immutable(key, bytes, mime).await
    }

    pub async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        self.inner.open(key, None).await
    }

    pub async fn hash_object(&self, key: &str, size: i64) -> Result<(String, Option<u8>)> {
        self.transfer(key, size, tokio::io::sink()).await
    }

    pub async fn copy_verified(
        &self,
        key: &str,
        size: i64,
        sha: &str,
        path: &std::path::Path,
    ) -> Result<()> {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await?;
        let mut file = tokio::io::BufWriter::new(file);
        let (actual, _) = self.transfer(key, size, &mut file).await?;
        anyhow::ensure!(actual == sha, ObjectMismatch);
        file.into_inner().sync_all().await?;
        Ok(())
    }

    async fn transfer(
        &self,
        key: &str,
        size: i64,
        mut output: impl AsyncWrite + Unpin,
    ) -> Result<(String, Option<u8>)> {
        let expected = u64::try_from(size)?;
        let mut count = 0u64;
        let mut first = None;
        let mut digest = Sha256::new();
        let mut chunks = self.inner.stream(key).await?;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            count += chunk.len() as u64;
            anyhow::ensure!(count <= expected, ObjectMismatch);
            first = first.or_else(|| chunk.first().copied());
            digest.update(&chunk);
            output.write_all(&chunk).await?;
        }
        anyhow::ensure!(count == expected, ObjectMismatch);
        output.flush().await?;
        Ok((hex::encode(digest.finalize()), first))
    }

    pub async fn read_bounded(&self, key: &str, limit: usize) -> Result<Vec<u8>> {
        let mut chunks = self.inner.stream(key).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            anyhow::ensure!(chunk.len() <= limit - bytes.len(), "storage object exceeds limit");
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    pub async fn walk_owned(&self, after: Option<&str>, limit: i64) -> Result<Vec<String>> {
        self.inner.walk_owned(after, limit).await
    }

    pub async fn delete_key(&self, key: &str) -> Result<()> {
        self.inner.delete(key).await
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendConfig {
    Local { root: PathBuf },
    S3 {
        endpoint: Option<String>,
        region: String,
        bucket: String,
        prefix: String,
        access_key: Option<String>,
        secret_key: Option<String>,
    },
    WebDav {
        origin: String,
        collection: String,
        username: Option<String>,
        password: Option<String>,
    },
}

pub fn original_key(image_id: &str, sha: &str, mime: &str) -> String {
    let ext = mime_ext(mime);
    format!(
        "media/{}/{}/{}.{}",
        image_dir_prefix(image_id),
        image_id,
        sha,
        ext
    )
}

pub fn variant_key(image_id: &str, spec: &str, revision: i64, sha: &str) -> String {
    format!(
        "media/{}/{}/{}-r{}_{}.png",
        image_dir_prefix(image_id),
        image_id,
        spec,
        revision,
        sha
    )
}

pub fn stage_key(item_id: &str, attempt: &str) -> String {
    format!("work/stage/{item_id}/{attempt}")
}

pub fn archive_key(archive_id: &str) -> String {
    format!("work/archives/{archive_id}.zip")
}

pub fn mime_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "bin",
    }
}

pub fn detect_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg");
    }
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 2 && bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if bytes.len() >= 4 && (bytes.starts_with(&[0x49, 0x49, 0x2a, 0x00]) || bytes.starts_with(&[0x4d, 0x4d, 0x00, 0x2a])) {
        return Some("image/tiff");
    }
    None
}

pub async fn record_blob(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    sha: &str,
    size: i64,
    mime: &str,
    kind: &str,
    backend_id: &str,
    object_key: &str,
) -> Result<String> {
    if let Some((id,)) = sqlx::query_as::<_, (String,)>(
        "SELECT b.id FROM media_blobs b
         JOIN media_locations l ON l.blob_id = b.id
         WHERE b.sha256 = ? AND b.size = ? AND l.state = 'verified' LIMIT 1",
    )
    .bind(sha)
    .bind(size)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(id);
    }
    let id = new_id();
    let now = crate::db::now_ms();
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES(?, ?, ?, ?, 'verified', ?, ?)",
    )
    .bind(&id)
    .bind(sha)
    .bind(size)
    .bind(mime)
    .bind(kind)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, backend_revision, size, sha256, state, verified_at)
         VALUES(?, ?, ?, 1, ?, ?, 'verified', ?)",
    )
    .bind(&id)
    .bind(backend_id)
    .bind(object_key)
    .bind(size)
    .bind(sha)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

pub fn is_missing(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.to_string().contains("storage missing"))
}

pub async fn mark_missing(pool: &SqlitePool, backend_id: &str, blob_id: &str) -> Result<()> {
    let mut tx = crate::db::begin_immediate(pool).await?;
    sqlx::query(
        "UPDATE media_locations SET state = 'failed'
         WHERE blob_id = ? AND backend_id = ? AND state = 'verified'",
    )
    .bind(blob_id)
    .bind(backend_id)
    .execute(&mut *tx)
    .await?;
    let variants = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT v.image_id, v.spec_id, v.profile_revision
         FROM image_variants v
         JOIN images i ON i.id = v.image_id
         WHERE v.blob_id = ? AND v.state = 'published' AND i.state = 'active'",
    )
    .bind(blob_id)
    .fetch_all(&mut *tx)
    .await?;
    for (image_id, spec_id, revision) in variants {
        let payload = crate::jobs::JobPayload {
            kind: "generate_variant".into(),
            payload_version: 1,
            scope: serde_json::json!({"image_id": image_id}),
            target: serde_json::json!({
                "profile_revision": revision,
                "spec_id": spec_id
            }),
            actor_id: None,
        };
        crate::jobs::enqueue(
            &mut tx,
            "generate_variant",
            &format!("rebuild_variant:{blob_id}:{spec_id}"),
            &payload,
            None,
            if spec_id == "thumb" { 100 } else { 10 },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn location_for(
    pool: &SqlitePool,
    blob_id: &str,
    backend_id: &str,
) -> Result<(String, i64, String, String)> {
    sqlx::query_as(
        "SELECT l.object_key, l.size, l.sha256, b.mime FROM media_locations l
         JOIN media_blobs b ON b.id = l.blob_id
         WHERE l.blob_id = ? AND l.backend_id = ? AND l.state = 'verified'",
    )
    .bind(blob_id)
    .bind(backend_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| anyhow!("missing verified location"))
}

pub async fn open_by_id(pool: &SqlitePool, backend_id: &str) -> Result<Storage> {
    let (json,): (String,) = sqlx::query_as("SELECT config_json FROM storage_backends WHERE id = ?")
        .bind(backend_id)
        .fetch_one(pool)
        .await
        .context("storage backend")?;
    let storage = open_backend(backend_id.to_string(), serde_json::from_str(&json)?)?;
    storage.backend().bind_catalog(pool.clone(), storage.backend_id.clone());
    Ok(storage)
}

pub async fn open_routed(pool: &SqlitePool) -> Result<Storage> {
    let (id, json): (String, String) = sqlx::query_as(
        "SELECT b.id, b.config_json FROM storage_route r
         JOIN storage_backends b ON b.id = r.active_backend_id
         WHERE r.id = 1",
    )
    .fetch_one(pool)
    .await
    .context("storage route")?;
    let storage = open_backend(id.clone(), serde_json::from_str(&json)?)?;
    storage.backend().bind_catalog(pool.clone(), id);
    Ok(storage)
}

pub async fn active_backend(pool: &SqlitePool) -> Result<(String, String)> {
    sqlx::query_as(
        "SELECT r.active_backend_id, b.kind FROM storage_route r
         JOIN storage_backends b ON b.id = r.active_backend_id
         WHERE r.id = 1",
    )
    .fetch_one(pool)
    .await
    .context("storage route")
}

pub async fn bootstrap_local(pool: &SqlitePool, media_root: PathBuf) -> Result<Storage> {
    let existing: Option<(String, String)> =
        sqlx::query_as("SELECT id, config_json FROM storage_backends WHERE kind = 'local' LIMIT 1")
            .fetch_optional(pool)
            .await?;
    let (backend_id, cfg) = if let Some((id, json)) = existing {
        (id, serde_json::from_str::<BackendConfig>(&json)?)
    } else {
        let id = new_id();
        let cfg = BackendConfig::Local {
            root: media_root.clone(),
        };
        let now = crate::db::now_ms();
        sqlx::query(
            "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
             VALUES(?, 'local', ?, 1, ?)",
        )
        .bind(&id)
        .bind(serde_json::to_string(&cfg)?)
        .bind(now)
        .execute(pool)
        .await?;
        let route: Option<(i64,)> = sqlx::query_as("SELECT id FROM storage_route WHERE id = 1")
            .fetch_optional(pool)
            .await?;
        if route.is_none() {
            sqlx::query(
                "INSERT INTO storage_route(id, active_backend_id, routing_epoch, catalog_revision, write_barrier)
                 VALUES(1, ?, 1, 1, 0)",
            )
            .bind(&id)
            .execute(pool)
            .await?;
        }
        (id, cfg)
    };
    open_backend(backend_id, cfg)
}

pub fn open_backend(backend_id: String, cfg: BackendConfig) -> Result<Storage> {
    let inner: std::sync::Arc<dyn MediaBackend> = match cfg {
        BackendConfig::Local { root } => std::sync::Arc::new(local::LocalFs::new(root)),
        BackendConfig::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key,
            secret_key,
        } => std::sync::Arc::new(s3::S3Backend::new(
            endpoint, region, bucket, prefix, access_key, secret_key,
        )),
        BackendConfig::WebDav {
            origin,
            collection,
            username,
            password,
        } => std::sync::Arc::new(webdav::WebDavBackend::new(
            origin, collection, username, password,
        )),
    };
    Ok(Storage::new(backend_id, inner))
}

pub fn sha_of(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

pub struct MediaRead {
    pool: SqlitePool,
    holder: String,
}

impl Drop for MediaRead {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let holder = self.holder.clone();
        tokio::spawn(async move {
            let _ = release_media_read(&pool, &holder).await;
        });
    }
}

pub async fn pin_media_read(
    pool: &SqlitePool,
    backend_id: &str,
    routing_epoch: i64,
    blob_id: &str,
) -> Result<MediaRead> {
    let holder = new_id();
    sqlx::query(
        "INSERT INTO media_reads(holder, backend_id, routing_epoch, blob_id, until_ms)
         VALUES(?, ?, ?, ?, ?)",
    )
    .bind(&holder)
    .bind(backend_id)
    .bind(routing_epoch)
    .bind(blob_id)
    .bind(crate::db::now_ms() + 24 * 60 * 60 * 1000)
    .execute(pool)
    .await?;
    Ok(MediaRead {
        pool: pool.clone(),
        holder,
    })
}

pub async fn release_media_read(pool: &SqlitePool, holder: &str) -> Result<()> {
    sqlx::query("DELETE FROM media_reads WHERE holder = ?")
        .bind(holder)
        .execute(pool)
        .await?;
    Ok(())
}
