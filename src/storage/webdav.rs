use std::collections::HashSet;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, RANGE};
use reqwest::{Client, Method, StatusCode, Url};

use super::{BackendCaps, MediaBackend, ObjectMeta, StagedObject};
use crate::ids::sha256_hex;

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

pub struct WebDavBackend {
    origin: String,
    collection: String,
    username: Option<String>,
    password: Option<String>,
    client: Client,
}

impl WebDavBackend {
    pub fn new(
        origin: String,
        collection: String,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        Self {
            origin: origin.trim_end_matches('/').to_string(),
            collection: collection.trim_matches('/').to_string(),
            username,
            password,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest"),
        }
    }

    fn object_url(&self, key: &str) -> Result<Url> {
        let path = format!("{}/{}/{}", self.origin, self.collection, key);
        Url::parse(&path).map_err(|e| anyhow!(e))
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => req.basic_auth(u, Some(p)),
            _ => req,
        }
    }

    async fn ensure_parents(&self, key: &str) -> Result<()> {
        let mut acc = String::new();
        for part in key.split('/') {
            if part.is_empty() {
                continue;
            }
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if key.ends_with(part) && !key.ends_with('/') && key.rsplit('/').next() == Some(part) {
                break;
            }
        }
        let mut prefix = String::new();
        let segs: Vec<&str> = key.split('/').collect();
        for seg in &segs[..segs.len().saturating_sub(1)] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let url = self.object_url(&format!("{prefix}/"))?;
            let req = self.apply_auth(self.client.request(Method::from_bytes(b"MKCOL")?, url));
            let res = req.send().await?;
            if !(res.status().is_success()
                || res.status() == StatusCode::METHOD_NOT_ALLOWED
                || res.status() == StatusCode::CONFLICT
                || res.status() == StatusCode::CREATED)
            {
                if res.status() != StatusCode::MOVED_PERMANENTLY {
                    tracing::debug!(status = %res.status(), "mkcol");
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MediaBackend for WebDavBackend {
    fn kind(&self) -> &'static str {
        "webdav"
    }

    async fn probe(&self) -> Result<BackendCaps> {
        let key = "_probe/gallery-probe.bin";
        self.put_immutable(key, b"gallery-probe", "application/octet-stream")
            .await?;
        self.delete(key).await?;
        Ok(BackendCaps {
            range: true,
            conditional_put: false,
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
        self.ensure_parents(key).await?;
        let url = self.object_url(key)?;
        let file = tokio::fs::File::open(path).await?;
        let res = self
            .apply_auth(self.client.put(url))
            .header(CONTENT_TYPE, mime)
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(file)
            .send()
            .await?;
        if !res.status().is_success() && res.status() != StatusCode::CREATED {
            return Err(anyhow!("webdav put {}", res.status()));
        }
        Ok(ObjectMeta {
            size,
            sha256: sha,
            mime: mime.to_string(),
        })
    }

    async fn put_immutable(&self, key: &str, bytes: &[u8], mime: &str) -> Result<ObjectMeta> {
        self.ensure_parents(key).await?;
        let url = self.object_url(key)?;
        let req = self
            .apply_auth(self.client.put(url))
            .header(CONTENT_TYPE, mime)
            .body(bytes.to_vec());
        let res = req.send().await?;
        if !res.status().is_success() && res.status() != StatusCode::CREATED {
            return Err(anyhow!("webdav put {}", res.status()));
        }
        Ok(ObjectMeta {
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            mime: mime.to_string(),
        })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let url = self.object_url(key)?;
        let res = self.apply_auth(self.client.head(url)).send().await?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(anyhow!("webdav head {}", res.status()));
        }
        Ok(Some(ObjectMeta {
            size: res.content_length().unwrap_or(0),
            sha256: String::new(),
            mime: res
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string(),
        }))
    }

    async fn open(&self, key: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        let url = self.object_url(key)?;
        let mut req = self.apply_auth(self.client.get(url));
        if let Some((start, end)) = range {
            req = req.header(RANGE, format!("bytes={start}-{end}"));
        }
        let res = req
            .send()
            .await
            .map_err(|err| anyhow!("storage unavailable: {err}"))?;
        if res.status() == StatusCode::NOT_FOUND {
            anyhow::bail!("storage missing");
        }
        if !res.status().is_success() && res.status() != StatusCode::PARTIAL_CONTENT {
            anyhow::bail!("storage unavailable: webdav get {}", res.status());
        }
        Ok(res
            .bytes()
            .await
            .map_err(|err| anyhow!("storage unavailable: {err}"))?
            .to_vec())
    }

    async fn stream(&self, key: &str) -> Result<super::ObjectStream> {
        use futures_util::TryStreamExt;
        let response = self.apply_auth(self.client.get(self.object_url(key)?)).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            anyhow::bail!("storage missing");
        }
        anyhow::ensure!(response.status() == StatusCode::OK,
            "storage unavailable: webdav get {}", response.status());
        Ok(Box::pin(response.bytes_stream().map_err(anyhow::Error::from)))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let url = self.object_url(key)?;
        let res = self.apply_auth(self.client.delete(url)).send().await?;
        if res.status() == StatusCode::NOT_FOUND || res.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("webdav delete {}", res.status()))
        }
    }

    async fn walk_owned(&self, after: Option<&str>, limit: i64) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut seen = HashSet::new();
        self.collect_files("", &mut keys, &mut seen).await?;
        keys.sort();
        keys.dedup();
        let limit = limit.clamp(1, 500) as usize;
        Ok(keys
            .into_iter()
            .filter(|key| after.is_none_or(|cursor| key.as_str() > cursor))
            .take(limit)
            .collect())
    }
}

impl WebDavBackend {
    fn collect_files<'a>(
        &'a self,
        dir: &'a str,
        keys: &'a mut Vec<String>,
        seen: &'a mut HashSet<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if !seen.insert(dir.to_string()) {
                return Ok(());
            }
            let request_key = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let url = self.object_url(&request_key)?;
            let res = self
                .apply_auth(self.client.request(Method::from_bytes(b"PROPFIND")?, url))
                .header("Depth", "1")
                .header(CONTENT_TYPE, "application/xml")
                .body(
                    r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/></D:prop></D:propfind>"#,
                )
                .send()
                .await?;
            if res.status() == StatusCode::MOVED_PERMANENTLY
                || res.status() == StatusCode::FOUND
                || res.status() == StatusCode::TEMPORARY_REDIRECT
                || res.status() == StatusCode::PERMANENT_REDIRECT
            {
                anyhow::bail!("webdav propfind redirect refused");
            }
            if res.status() != StatusCode::MULTI_STATUS && !res.status().is_success() {
                anyhow::bail!("webdav propfind {}", res.status());
            }
            let text = res.text().await?;
            let entries = propfind_entries(&text)?;
            let mut dirs = Vec::new();
            for (href, is_dir) in entries {
                let Some(logical) = self.href_logical(&href) else {
                    continue;
                };
                if logical.is_empty() || logical == dir {
                    continue;
                }
                if is_dir {
                    if dir.is_empty() || logical.starts_with(&format!("{dir}/")) {
                        dirs.push(logical);
                    }
                } else if owned_key(&logical) {
                    keys.push(logical);
                }
            }
            for child in dirs {
                self.collect_files(&child, keys, seen).await?;
            }
            Ok(())
        })
    }

    fn href_logical(&self, href: &str) -> Option<String> {
        let href = href.trim();
        let path = if href.starts_with("http://") || href.starts_with("https://") {
            let url = Url::parse(href).ok()?;
            let origin = Url::parse(&self.origin).ok()?;
            if url.scheme() != origin.scheme()
                || url.host_str() != origin.host_str()
                || url.port_or_known_default() != origin.port_or_known_default()
            {
                return None;
            }
            percent_decode(url.path())
        } else {
            percent_decode(href.split('?').next().unwrap_or(href))
        };
        let root = format!("/{}", self.collection.trim_matches('/'));
        if path == root || path == format!("{root}/") {
            return Some(String::new());
        }
        Some(path.strip_prefix(&format!("{root}/"))?.trim_matches('/').to_string())
    }
}

fn owned_key(key: &str) -> bool {
    !key.is_empty()
        && !key.ends_with('/')
        && !key.split('/').any(|part| part.is_empty() || part == "." || part == "..")
}

pub(crate) fn propfind_entries(body: &str) -> Result<Vec<(String, bool)>> {
    let lower = body.to_ascii_lowercase();
    if lower.contains("<!doctype") || lower.contains("<!entity") {
        anyhow::bail!("webdav propfind refused dtd");
    }
    let mut entries = Vec::new();
    let mut search = 0;
    while search < lower.len() {
        let Some(rel) = find_tag(&lower[search..], "response") else {
            break;
        };
        let start = search + rel;
        let Some(end_rel) = find_close(&lower[start..], "response") else {
            break;
        };
        let block = &body[start..start + end_rel];
        let block_lower = &lower[start..start + end_rel];
        if let Some(href) = tag_text(block, block_lower, "href") {
            entries.push((href, element_has(block_lower, "resourcetype", "collection")));
        }
        search = start + end_rel;
    }
    Ok(entries)
}

fn find_tag(lower: &str, name: &str) -> Option<usize> {
    let names = [format!("<d:{name}"), format!("<{name}")];
    names.iter().filter_map(|tag| lower.find(tag)).min()
}

fn find_close(lower: &str, name: &str) -> Option<usize> {
    let names = [format!("</d:{name}>"), format!("</{name}>")];
    names
        .iter()
        .filter_map(|tag| lower.find(tag).map(|at| at + tag.len()))
        .min()
}

fn tag_text(block: &str, lower: &str, name: &str) -> Option<String> {
    let start = find_tag(lower, name)?;
    let after = start + lower[start..].find('>')? + 1;
    let end = find_close(&lower[after..], name)? + after;
    let close_at = lower[after..end].rfind('<').map(|at| after + at).unwrap_or(end);
    Some(block[after..close_at].trim().to_string())
}

fn element_has(lower: &str, name: &str, needle: &str) -> bool {
    let Some(start) = find_tag(lower, name) else {
        return false;
    };
    let Some(gt) = lower[start..].find('>') else {
        return false;
    };
    let after = start + gt + 1;
    if lower[start..after].ends_with("/>") {
        return false;
    }
    let Some(end) = find_close(&lower[after..], name) else {
        return false;
    };
    lower[after..after + end].contains(needle)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&input[index + 1..index + 3], 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
