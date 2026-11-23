use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use super::{BackendCaps, MediaBackend, ObjectMeta, StagedObject};
use crate::ids::sha256_hex;

#[derive(Clone)]
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn resolve(&self, key: &str) -> Result<PathBuf> {
        if key.is_empty() || key.starts_with('/') || key.contains('\0') {
            bail!("invalid object key");
        }
        for part in key.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                bail!("invalid object key segment");
            }
        }
        let path = self.root.join(key);
        self.ensure_contained(&path)?;
        Ok(path)
    }

    fn ensure_contained(&self, path: &Path) -> Result<()> {
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        let mut current = self.root.clone();
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        for component in relative.components() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => bail!("symlink rejected"),
                Ok(_) => {
                    if let Ok(canon) = current.canonicalize() {
                        if !canon.starts_with(&root) {
                            bail!("path escapes storage root");
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => bail!("storage unavailable: {err}"),
            }
        }
        Ok(())
    }

    pub fn stream_copy(src: &Path, dest: &Path) -> Result<(u64, String)> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut input = std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
        let mut output = std::fs::File::create(dest)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0_u64;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            output.write_all(&buf[..n])?;
            total += n as u64;
        }
        output.sync_all()?;
        Ok((total, hex::encode(hasher.finalize())))
    }

    pub fn publish_local(partial: &Path, stable: &Path) -> Result<()> {
        if stable.is_file() {
            let (_, existing) = hash_file(stable)?;
            let (_, staged) = hash_file(partial)?;
            if existing == staged {
                let _ = std::fs::remove_file(partial);
                return Ok(());
            }
            bail!("immutable object already exists with different content");
        }
        if let Some(parent) = stable.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(partial, stable).or_else(|_| {
            std::fs::copy(partial, stable)?;
            std::fs::remove_file(partial)?;
            Ok::<(), std::io::Error>(())
        })?;
        if let Some(parent) = stable.parent() {
            let dir = std::fs::File::open(parent)?;
            dir.sync_all()?;
        }
        Ok(())
    }

    fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let existing = std::fs::read(path)?;
            if existing.as_slice() == bytes {
                return Ok(());
            }
            bail!("immutable object already exists with different content");
        }
        Self::write_new(path, bytes, crate::download::free_bytes(path))?;
        Ok(())
    }

    fn write_new(path: &Path, bytes: &[u8], free: Option<u64>) -> Result<()> {
        ensure_room(free, bytes.len() as u64)?;
        let tmp = path.with_extension("tmp-write");
        if let Err(err) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            if err.kind() == std::io::ErrorKind::StorageFull {
                bail!("DISK_FULL");
            }
            return Err(err.into());
        }
        let file = std::fs::OpenOptions::new().write(true).open(&tmp)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Some(parent) = path.parent() {
            let dir = std::fs::File::open(parent)?;
            dir.sync_all()?;
        }
        Ok(())
    }
}

#[async_trait]
impl MediaBackend for LocalFs {
    fn kind(&self) -> &'static str {
        "local"
    }

    async fn probe(&self) -> Result<BackendCaps> {
        std::fs::create_dir_all(&self.root)?;
        let key = "_probe/gallery-probe.bin";
        let payload = b"gallery-probe";
        self.put_immutable(key, payload, "application/octet-stream")
            .await?;
        let got = self.open(key, Some((0, 6))).await?;
        if got != b"gallery" {
            bail!("range probe mismatch");
        }
        self.delete(key).await?;
        Ok(BackendCaps {
            range: true,
            conditional_put: true,
            checksum: true,
            move_object: true,
        })
    }

    async fn stage(&self, key: &str, bytes: &[u8]) -> Result<StagedObject> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
        Ok(StagedObject {
            key: key.to_string(),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        })
    }

    async fn put_file(&self, key: &str, path: &std::path::Path, mime: &str) -> Result<ObjectMeta> {
        let dest = self.resolve(key)?;
        if dest.is_file() {
            let (_, existing) = hash_file(&dest)?;
            let (_, incoming) = hash_file(path)?;
            if existing == incoming {
                let meta = dest.metadata()?;
                return Ok(ObjectMeta {
                    size: meta.len(),
                    sha256: existing,
                    mime: mime.to_string(),
                });
            }
            bail!("immutable object already exists with different content");
        }
        let (size, sha) = Self::stream_copy(path, &dest)?;
        Ok(ObjectMeta {
            size,
            sha256: sha,
            mime: mime.to_string(),
        })
    }

    async fn put_immutable(&self, key: &str, bytes: &[u8], _mime: &str) -> Result<ObjectMeta> {
        let path = self.resolve(key)?;
        Self::write_atomic(&path, bytes).with_context(|| format!("put {}", path.display()))?;
        Ok(ObjectMeta {
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            mime: _mime.to_string(),
        })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let path = self.resolve(key)?;
        if !path.is_file() {
            return Ok(None);
        }
        let (size, sha256) = hash_file(&path)?;
        Ok(Some(ObjectMeta {
            size,
            sha256,
            mime: "application/octet-stream".into(),
        }))
    }

    async fn stream(&self, key: &str) -> Result<super::ObjectStream> {
        use futures_util::TryStreamExt;
        let path = self.resolve(key)?;
        let file = tokio::fs::File::open(path).await.map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => anyhow::anyhow!("storage missing"),
            _ => anyhow::anyhow!("storage unavailable: {err}"),
        })?;
        anyhow::ensure!(file.metadata().await?.is_file(), "storage unavailable: not a file");
        Ok(Box::pin(tokio_util::io::ReaderStream::new(file).map_err(anyhow::Error::from)))
    }

    fn local_file(&self, key: &str) -> Option<PathBuf> {
        self.resolve(key).ok()
    }

    async fn open(&self, key: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.resolve(key)?;
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("storage missing")
            }
            Err(err) => anyhow::bail!("storage unavailable: {err}"),
        };
        let meta = file.metadata().map_err(|err| anyhow::anyhow!("storage unavailable: {err}"))?;
        if !meta.is_file() {
            anyhow::bail!("storage unavailable: not a file");
        }
        let len = meta.len();
        let (start, end) = match range {
            None if len == 0 => return Ok(Vec::new()),
            None => (0, len - 1),
            Some((start, end)) => (start, end),
        };
        if start >= len || start > end {
            bail!("invalid range");
        }
        let end = end.min(len - 1);
        let want = (end - start + 1) as usize;
        file.seek(SeekFrom::Start(start))
            .map_err(|err| anyhow::anyhow!("storage unavailable: {err}"))?;
        let mut buf = vec![0u8; want];
        file.read_exact(&mut buf)
            .map_err(|err| anyhow::anyhow!("storage unavailable: {err}"))?;
        Ok(buf)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.resolve(key)?;
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    async fn walk_owned(&self, after: Option<&str>, limit: i64) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        if self.root.is_dir() {
            collect_files(&self.root, &self.root, &mut keys)?;
        }
        keys.sort();
        let limit = limit.clamp(1, 500) as usize;
        Ok(keys
            .into_iter()
            .filter(|key| after.is_none_or(|cursor| key.as_str() > cursor))
            .take(limit)
            .collect())
    }
}

pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex::encode(hasher.finalize())))
}

fn ensure_room(free: Option<u64>, bytes: u64) -> Result<()> {
    if free.is_some_and(|free| free < bytes) {
        bail!("DISK_FULL");
    }
    Ok(())
}

fn collect_files(root: &Path, dir: &Path, keys: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("walk {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_files(root, &path, keys)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let key = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if !key.is_empty() {
            keys.push(key);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_range_read_uses_the_requested_span() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path().to_path_buf());
        let bytes: Vec<u8> = (0..200).map(|n| n as u8).collect();
        fs.put_immutable("media/ab/span.bin", &bytes, "application/octet-stream")
            .await
            .unwrap();
        let middle = fs.open("media/ab/span.bin", Some((20, 24))).await.unwrap();
        assert_eq!(middle, bytes[20..=24]);
        let whole = fs.open("media/ab/span.bin", None).await.unwrap();
        assert_eq!(whole, bytes);
        let past = fs.open("media/ab/span.bin", Some((200, 200))).await;
        assert!(past.unwrap_err().to_string().contains("invalid range"));
        let missing = fs.open("media/ab/missing.bin", None).await;
        assert!(missing.unwrap_err().to_string().contains("storage missing"));
        std::fs::create_dir(dir.path().join("media/ab/not-a-file")).unwrap();
        let dir_read = fs.open("media/ab/not-a-file", None).await;
        assert!(dir_read.unwrap_err().to_string().contains("storage unavailable"));
        let outside = dir.path().join("outside.bin");
        std::fs::write(&outside, b"secret-outside").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("media/ab/linked.bin")).unwrap();
        let linked = fs.open("media/ab/linked.bin", None).await;
        assert!(linked.unwrap_err().to_string().contains("symlink rejected"));
        let wrote = fs
            .put_immutable("media/ab/linked.bin", b"nope", "application/octet-stream")
            .await;
        assert!(wrote.unwrap_err().to_string().contains("symlink rejected"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"secret-outside");
        let blocked = dir.path().join("blocked.bin");
        let err = LocalFs::write_new(&blocked, b"0123456789", Some(4)).unwrap_err();
        assert!(err.to_string().contains("DISK_FULL"));
        assert!(!err.to_string().contains("permanent:"));
        assert!(!blocked.exists());
        assert!(!blocked.with_extension("tmp-write").exists());
        LocalFs::write_new(&blocked, b"0123456789", Some(10)).unwrap();
        assert_eq!(std::fs::read(&blocked).unwrap(), b"0123456789");
        let escaped = fs.open("media/../outside.bin", None).await;
        assert!(escaped.unwrap_err().to_string().contains("invalid object key"));
    }
}
