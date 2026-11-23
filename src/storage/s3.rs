use anyhow::{anyhow, Result};
use async_trait::async_trait;
use aws_sdk_s3::config::{Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    AbortIncompleteMultipartUpload, BucketLifecycleConfiguration, CompletedMultipartUpload, CompletedPart,
    ExpirationStatus, LifecycleRule, LifecycleRuleFilter,
};
use aws_sdk_s3::Client;

use super::{BackendCaps, MediaBackend, ObjectMeta, StagedObject};
use crate::ids::sha256_hex;

const DEFAULT_PART_BYTES: usize = 8 * 1024 * 1024;

fn hash_path(path: &std::path::Path) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex::encode(hasher.finalize())))
}

pub struct S3Backend {
    endpoint: Option<String>,
    region: String,
    bucket: String,
    prefix: String,
    access_key: Option<String>,
    secret_key: Option<String>,
    multipart_threshold: usize,
    part_bytes: usize,
    catalog: std::sync::Mutex<Option<(sqlx::SqlitePool, String)>>,
}

impl S3Backend {
    pub fn new(
        endpoint: Option<String>,
        region: String,
        bucket: String,
        prefix: String,
        access_key: Option<String>,
        secret_key: Option<String>,
    ) -> Self {
        Self {
            endpoint,
            region,
            bucket,
            prefix,
            access_key,
            secret_key,
            multipart_threshold: DEFAULT_PART_BYTES,
            part_bytes: DEFAULT_PART_BYTES,
            catalog: std::sync::Mutex::new(None),
        }
    }

    pub fn with_multipart_threshold(mut self, threshold: usize, part_bytes: usize) -> Self {
        self.multipart_threshold = threshold.max(1);
        self.part_bytes = part_bytes.max(1);
        self
    }

    async fn client(&self) -> Result<Client> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(Region::new(self.region.clone()));
        if let (Some(ak), Some(sk)) = (&self.access_key, &self.secret_key) {
            loader = loader.credentials_provider(Credentials::new(
                ak,
                sk,
                None,
                None,
                "gallery",
            ));
        }
        let shared = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&shared)
            .force_path_style(true)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        if let Some(ep) = &self.endpoint {
            builder = builder.endpoint_url(ep);
        }
        Ok(Client::from_conf(builder.build()))
    }

    fn key(&self, object_key: &str) -> String {
        let prefix = self.prefix.trim_matches('/');
        if prefix.is_empty() {
            object_key.to_string()
        } else {
            format!("{prefix}/{object_key}")
        }
    }

    fn list_prefix(&self) -> String {
        let prefix = self.prefix.trim_matches('/');
        if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        }
    }

    pub async fn ensure_multipart_lifecycle(&self) -> Result<i32> {
        let client = self.client().await?;
        let prefix = self.list_prefix();
        let rule = LifecycleRule::builder()
            .id("gallery-abort-incomplete-multipart")
            .status(ExpirationStatus::Enabled)
            .filter(LifecycleRuleFilter::builder().prefix(prefix.clone()).build())
            .abort_incomplete_multipart_upload(
                AbortIncompleteMultipartUpload::builder()
                    .days_after_initiation(1)
                    .build(),
            )
            .build()?;
        client
            .put_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .lifecycle_configuration(BucketLifecycleConfiguration::builder().rules(rule).build()?)
            .send()
            .await
            .map_err(|err| anyhow!("s3 lifecycle put failed: {err}"))?;
        let read = client
            .get_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .send()
            .await
            .map_err(|err| anyhow!("s3 lifecycle read failed: {err}"))?;
        let days = read.rules().iter().find_map(|rule| {
            if rule.status() != &ExpirationStatus::Enabled {
                return None;
            }
            let rule_prefix = rule.filter().and_then(|filter| filter.prefix()).unwrap_or("");
            let covers = rule_prefix.is_empty() || prefix.starts_with(rule_prefix);
            let days = rule
                .abort_incomplete_multipart_upload()
                .and_then(|abort| abort.days_after_initiation())?;
            (covers && days >= 1).then_some(days)
        });
        days.ok_or_else(|| anyhow!("s3 lifecycle rule missing"))
    }

    fn logical_key(&self, full: &str) -> Option<String> {
        let prefix = self.list_prefix();
        let rest = if prefix.is_empty() {
            full
        } else {
            full.strip_prefix(&prefix)?
        };
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }
    }
}

#[async_trait]
impl MediaBackend for S3Backend {
    fn kind(&self) -> &'static str {
        "s3"
    }

    async fn probe(&self) -> Result<BackendCaps> {
        self.ensure_multipart_lifecycle().await?;
        let client = self.client().await?;
        let key = self.key("_probe/gallery-probe.bin");
        client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(ByteStream::from_static(b"gallery-probe"))
            .if_none_match("*")
            .send()
            .await
            .ok();
        client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await?;
        Ok(BackendCaps {
            range: true,
            conditional_put: true,
            checksum: true,
            move_object: false,
        })
    }

    async fn stage(&self, key: &str, bytes: &[u8]) -> Result<StagedObject> {
        let meta = self
            .put_immutable(key, bytes, "application/octet-stream")
            .await?;
        Ok(StagedObject {
            key: key.to_string(),
            size: meta.size,
            sha256: meta.sha256,
        })
    }

    async fn put_file(&self, key: &str, path: &std::path::Path, mime: &str) -> Result<ObjectMeta> {
        let (size, sha) = hash_path(path)?;
        if size >= self.multipart_threshold as u64 {
            return self.put_file_multipart(key, path, mime, size, &sha).await;
        }
        let bytes = std::fs::read(path)?;
        self.put_single(key, &bytes, mime).await
    }

    async fn put_immutable(&self, key: &str, bytes: &[u8], mime: &str) -> Result<ObjectMeta> {
        if bytes.len() >= self.multipart_threshold {
            return self.put_multipart(key, bytes, mime).await;
        }
        self.put_single(key, bytes, mime).await
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let client = self.client().await?;
        let full = self.key(key);
        match client
            .head_object()
            .bucket(&self.bucket)
            .key(&full)
            .send()
            .await
        {
            Ok(out) => Ok(Some(ObjectMeta {
                size: out.content_length().unwrap_or(0) as u64,
                sha256: out
                    .checksum_sha256()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                mime: out.content_type().unwrap_or("application/octet-stream").to_string(),
            })),
            Err(_) => Ok(None),
        }
    }

    async fn open(&self, key: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        let client = self.client().await?;
        let full = self.key(key);
        let mut req = client.get_object().bucket(&self.bucket).key(&full);
        if let Some((start, end)) = range {
            req = req.range(format!("bytes={start}-{end}"));
        }
        match req.send().await {
            Ok(out) => Ok(out
                .body
                .collect()
                .await
                .map_err(|err| anyhow!("storage unavailable: {err}"))?
                .into_bytes()
                .to_vec()),
            Err(err) => {
                let text = err.to_string();
                if text.contains("NoSuchKey") || text.contains("NotFound") || text.contains("404") {
                    anyhow::bail!("storage missing");
                }
                anyhow::bail!("storage unavailable: {text}");
            }
        }
    }

    async fn stream(&self, key: &str) -> Result<super::ObjectStream> {
        use futures_util::TryStreamExt;
        let response = self.client().await?.get_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .send()
            .await
            .map_err(|err| {
                if err.as_service_error().is_some_and(|err| err.is_no_such_key())
                    || err.raw_response().is_some_and(|response| response.status().as_u16() == 404)
                {
                    anyhow!("storage missing")
                } else {
                    anyhow!("storage unavailable: {err}")
                }
            })?;
        Ok(Box::pin(tokio_util::io::ReaderStream::new(response.body.into_async_read())
            .map_err(anyhow::Error::from)))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let client = self.client().await?;
        client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .send()
            .await?;
        Ok(())
    }

    async fn walk_owned(&self, after: Option<&str>, limit: i64) -> Result<Vec<String>> {
        let client = self.client().await?;
        let want = limit.clamp(1, 500) as usize;
        let prefix = self.list_prefix();
        let mut start_after = after.map(|key| self.key(key));
        let mut keys = Vec::new();
        for _ in 0..32 {
            let mut req = client
                .list_objects_v2()
                .bucket(&self.bucket)
                .max_keys((want - keys.len()).clamp(1, 500) as i32);
            if !prefix.is_empty() {
                req = req.prefix(&prefix);
            }
            if let Some(cursor) = &start_after {
                req = req.start_after(cursor);
            }
            let out = req
                .send()
                .await
                .map_err(|err| anyhow!("s3 list failed: {err}"))?;
            let batch = out.contents();
            if batch.is_empty() {
                break;
            }
            let mut last = None;
            for object in batch {
                let Some(full) = object.key() else { continue };
                last = Some(full.to_string());
                if let Some(logical) = self.logical_key(full) {
                    if owned_key(&logical) {
                        keys.push(logical);
                    }
                }
                if keys.len() >= want {
                    return Ok(keys);
                }
            }
            if !out.is_truncated().unwrap_or(false) {
                break;
            }
            let Some(last) = last else { break };
            if start_after.as_deref() == Some(last.as_str()) {
                break;
            }
            start_after = Some(last);
        }
        Ok(keys)
    }

    fn bind_catalog(&self, pool: sqlx::SqlitePool, backend_id: String) {
        self.store_catalog(pool, backend_id);
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        self.abort_upload(key, upload_id).await
    }

    async fn abort_stale_multiparts(&self, cutoff_ms: i64) -> Result<u64> {
        self.abort_listed_before(cutoff_ms).await
    }
}

fn owned_key(key: &str) -> bool {
    !key.is_empty()
        && !key.ends_with('/')
        && !key.split('/').any(|part| part.is_empty() || part == "." || part == "..")
}

impl S3Backend {
    async fn put_single(&self, key: &str, bytes: &[u8], mime: &str) -> Result<ObjectMeta> {
        let client = self.client().await?;
        let full = self.key(key);
        let sha = sha256_hex(bytes);
        let put = client
            .put_object()
            .bucket(&self.bucket)
            .key(&full)
            .content_type(mime)
            .body(ByteStream::from(bytes.to_vec()))
            .if_none_match("*")
            .send()
            .await;
        match put {
            Ok(_) => Ok(ObjectMeta {
                size: bytes.len() as u64,
                sha256: sha,
                mime: mime.to_string(),
            }),
            Err(_) => {
                let existing = self.head(key).await?;
                match existing {
                    Some(meta) if meta.size == bytes.len() as u64 => {
                        let got = self.open(key, None).await?;
                        if sha256_hex(&got) == sha {
                            Ok(ObjectMeta {
                                size: meta.size,
                                sha256: sha,
                                mime: meta.mime,
                            })
                        } else {
                            Err(anyhow!("s3 object exists with different content"))
                        }
                    }
                    Some(_) => Err(anyhow!("s3 object exists with different content")),
                    None => Err(anyhow!("s3 put failed")),
                }
            }
        }
    }

    async fn reuse_if_same(&self, key: &str, bytes: &[u8], mime: &str) -> Result<ObjectMeta> {
        let sha = sha256_hex(bytes);
        match self.head(key).await? {
            Some(meta) if meta.size == bytes.len() as u64 => {
                let got = self.open(key, None).await?;
                if sha256_hex(&got) == sha {
                    Ok(ObjectMeta {
                        size: meta.size,
                        sha256: sha,
                        mime: if meta.mime.is_empty() { mime.to_string() } else { meta.mime },
                    })
                } else {
                    Err(anyhow!("s3 object exists with different content"))
                }
            }
            Some(_) => Err(anyhow!("s3 object exists with different content")),
            None => Err(anyhow!("s3 put failed")),
        }
    }

    async fn put_multipart(&self, key: &str, bytes: &[u8], mime: &str) -> Result<ObjectMeta> {
        if self.head(key).await?.is_some() {
            return self.reuse_if_same(key, bytes, mime).await;
        }
        let client = self.client().await?;
        let full = self.key(key);
        let sha = sha256_hex(bytes);
        let created = client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&full)
            .content_type(mime)
            .send()
            .await
            .map_err(|err| anyhow!("s3 multipart start failed: {err}"))?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| anyhow!("s3 multipart missing upload id"))?
            .to_string();
        tracing::info!(upload_id = %upload_id, object_key = %key, "s3 multipart open");
        if let Err(err) = self.remember_upload(&upload_id, key).await {
            let _ = client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&full)
                .upload_id(&upload_id)
                .send()
                .await;
            return Err(err);
        }
        match self.finish_multipart(&client, &full, &upload_id, bytes).await {
            Ok(()) => {
                let got = self.open(key, None).await?;
                if got.len() != bytes.len() || sha256_hex(&got) != sha {
                    let _ = self.delete(key).await;
                    self.forget_upload(&upload_id).await;
                    anyhow::bail!("s3 multipart verification failed");
                }
                self.forget_upload(&upload_id).await;
                Ok(ObjectMeta {
                    size: bytes.len() as u64,
                    sha256: sha,
                    mime: mime.to_string(),
                })
            }
            Err(err) => {
                if client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(&full)
                    .upload_id(&upload_id)
                    .send()
                    .await
                    .is_ok()
                {
                    self.forget_upload(&upload_id).await;
                }
                Err(err)
            }
        }
    }

    fn store_catalog(&self, pool: sqlx::SqlitePool, backend_id: String) {
        *self.catalog.lock().unwrap() = Some((pool, backend_id));
    }

    async fn abort_upload(&self, key: &str, upload_id: &str) -> Result<()> {
        let client = self.client().await?;
        client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key(key))
            .upload_id(upload_id)
            .send()
            .await
            .map_err(|err| anyhow!("s3 multipart abort failed: {err}"))?;
        Ok(())
    }

    async fn remember_upload(&self, upload_id: &str, key: &str) -> Result<()> {
        let bound = self.catalog.lock().unwrap().clone();
        let Some((pool, backend_id)) = bound else {
            return Ok(());
        };
        sqlx::query(
            "INSERT INTO storage_multipart_uploads(upload_id, backend_id, object_key, opened_at)
             VALUES(?, ?, ?, ?)
             ON CONFLICT(upload_id) DO UPDATE SET opened_at = excluded.opened_at",
        )
        .bind(upload_id)
        .bind(backend_id)
        .bind(key)
        .bind(crate::db::now_ms())
        .execute(&pool)
        .await
        .map_err(|err| anyhow!("s3 multipart record failed: {err}"))?;
        Ok(())
    }

    async fn abort_listed_before(&self, cutoff_ms: i64) -> Result<u64> {
        let client = self.client().await?;
        let prefix = self.list_prefix();
        let mut aborted = 0u64;
        let mut key_marker: Option<String> = None;
        let mut upload_marker: Option<String> = None;
        loop {
            let mut request = client.list_multipart_uploads().bucket(&self.bucket).prefix(&prefix);
            if let Some(marker) = &key_marker {
                request = request.key_marker(marker);
            }
            if let Some(marker) = &upload_marker {
                request = request.upload_id_marker(marker);
            }
            let page = request
                .send()
                .await
                .map_err(|err| anyhow!("s3 list multipart failed: {err}"))?;
            for upload in page.uploads() {
                let (Some(upload_id), Some(full_key), Some(initiated)) =
                    (upload.upload_id(), upload.key(), upload.initiated())
                else {
                    continue;
                };
                if initiated.secs() * 1000 >= cutoff_ms {
                    continue;
                }
                client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(full_key)
                    .upload_id(upload_id)
                    .send()
                    .await
                    .map_err(|err| anyhow!("s3 multipart abort failed: {err}"))?;
                aborted += 1;
            }
            if page.is_truncated() != Some(true) {
                break;
            }
            key_marker = page.next_key_marker().map(str::to_string);
            upload_marker = page.next_upload_id_marker().map(str::to_string);
            if key_marker.is_none() && upload_marker.is_none() {
                break;
            }
        }
        Ok(aborted)
    }

    async fn forget_upload(&self, upload_id: &str) {
        let bound = self.catalog.lock().unwrap().clone();
        let Some((pool, _)) = bound else {
            return;
        };
        let _ = sqlx::query("DELETE FROM storage_multipart_uploads WHERE upload_id = ?")
            .bind(upload_id)
            .execute(&pool)
            .await;
    }

    async fn put_file_multipart(
        &self,
        key: &str,
        path: &std::path::Path,
        mime: &str,
        size: u64,
        sha: &str,
    ) -> Result<ObjectMeta> {
        let client = self.client().await?;
        let full = self.key(key);
        let created = client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&full)
            .content_type(mime)
            .send()
            .await
            .map_err(|err| anyhow!("s3 multipart start failed: {err}"))?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| anyhow!("s3 multipart missing upload id"))?
            .to_string();
        if let Err(err) = self.remember_upload(&upload_id, key).await {
            let _ = client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&full)
                .upload_id(&upload_id)
                .send()
                .await;
            return Err(err);
        }
        if let Err(err) = self.upload_file_parts(&client, &full, &upload_id, path).await {
            if client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&full)
                .upload_id(&upload_id)
                .send()
                .await
                .is_ok()
            {
                self.forget_upload(&upload_id).await;
            }
            return Err(err);
        }
        if !self.ranged_sha_matches(key, size, sha).await? {
            let _ = self.delete(key).await;
            self.forget_upload(&upload_id).await;
            anyhow::bail!("s3 multipart verification failed");
        }
        self.forget_upload(&upload_id).await;
        Ok(ObjectMeta {
            size,
            sha256: sha.to_string(),
            mime: mime.to_string(),
        })
    }

    async fn upload_file_parts(&self, client: &Client, full: &str, upload_id: &str, path: &std::path::Path) -> Result<()> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut parts = Vec::new();
        let mut number = 1i32;
        let mut buf = vec![0u8; self.part_bytes];
        loop {
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = file.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let uploaded = client
                .upload_part()
                .bucket(&self.bucket)
                .key(full)
                .upload_id(upload_id)
                .part_number(number)
                .body(ByteStream::from(buf[..filled].to_vec()))
                .send()
                .await
                .map_err(|err| anyhow!("s3 upload part failed: {err}"))?;
            let etag = uploaded.e_tag().unwrap_or("\"part\"").to_string();
            parts.push(CompletedPart::builder().part_number(number).e_tag(etag).build());
            number += 1;
        }
        client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(full)
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .send()
            .await
            .map_err(|err| anyhow!("s3 multipart complete failed: {err}"))?;
        Ok(())
    }

    async fn ranged_sha_matches(&self, key: &str, size: u64, sha: &str) -> Result<bool> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        while offset < size {
            let end = (offset + self.part_bytes as u64 - 1).min(size - 1);
            let chunk = self.open(key, Some((offset, end))).await?;
            hasher.update(&chunk);
            offset = end + 1;
        }
        Ok(hex::encode(hasher.finalize()) == sha)
    }

    async fn finish_multipart(&self, client: &Client, full: &str, upload_id: &str, bytes: &[u8]) -> Result<()> {
        let mut parts = Vec::new();
        let mut offset = 0usize;
        let mut number = 1i32;
        while offset < bytes.len() {
            let end = (offset + self.part_bytes).min(bytes.len());
            let chunk = bytes[offset..end].to_vec();
            let uploaded = client
                .upload_part()
                .bucket(&self.bucket)
                .key(full)
                .upload_id(upload_id)
                .part_number(number)
                .body(ByteStream::from(chunk))
                .send()
                .await
                .map_err(|err| anyhow!("s3 upload part failed: {err}"))?;
            let etag = uploaded.e_tag().unwrap_or("\"part\"").to_string();
            parts.push(CompletedPart::builder().part_number(number).e_tag(etag).build());
            offset = end;
            number += 1;
        }
        client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(full)
            .upload_id(upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .send()
            .await
            .map_err(|err| anyhow!("s3 multipart complete failed: {err}"))?;
        Ok(())
    }
}
