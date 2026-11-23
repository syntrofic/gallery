use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, ensure, Context, Result};
use serde_json::Value;

use crate::backup::{age_decrypt_bytes, age_identity, age_recipient, verify_complete_version};
use crate::ids::sha256_hex;
use crate::storage::local::hash_file;

pub const LEGACY_FORMAT: i64 = 1;
pub const FORMAT: i64 = 2;
pub const MANIFEST: &str = "manifest.json.age";
pub const CONFIG: &str = "config.json.age";
pub const SEAL: &str = "sealed-plan.json.age";

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("package directory")?)?;
    file.write_all(bytes)?;
    publish(file, path)
}

fn publish(file: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(path.parent().context("package directory")?)?.sync_all()?;
    Ok(())
}

pub fn encrypt_file(keys: &Path, source: &Path, target: &Path) -> Result<(u64, String)> {
    let recipient = age_recipient(keys)?;
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))?;
    let mut input = BufReader::new(std::fs::File::open(source)?);
    let mut output =
        tempfile::NamedTempFile::new_in(target.parent().context("package directory")?)?;
    let mut encrypted = encryptor.wrap_output(output.as_file_mut())?;
    std::io::copy(&mut input, &mut encrypted)?;
    encrypted.finish()?;
    let digest = hash_file(output.path())?;
    publish(output, target)?;
    Ok(digest)
}

pub fn decrypt_file(
    keys: &Path,
    source: &Path,
    dest: &Path,
    ciphertext_sha: Option<&str>,
    expected: Option<(i64, &str)>,
) -> Result<()> {
    if let Some(sha) = ciphertext_sha {
        ensure!(
            hash_file(source)?.1 == sha,
            "backup ciphertext hash mismatch"
        );
    }
    let mut output = tempfile::NamedTempFile::new_in(dest.parent().context("restore directory")?)?;
    let input = BufReader::new(std::fs::File::open(source)?);
    let identity = age_identity(keys, false)?;
    let decryptor = age::Decryptor::new_buffered(input)?;
    let mut plain = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?;
    if let Some((size, sha)) = expected {
        let limit = u64::try_from(size)?
            .checked_add(1)
            .context("backup object size")?;
        std::io::copy(&mut plain.take(limit), output.as_file_mut())?;
        let (actual_size, actual_sha) = hash_file(output.path())?;
        ensure!(
            actual_size == size as u64 && actual_sha == sha,
            "backup object hash mismatch"
        );
    } else {
        std::io::copy(&mut plain, output.as_file_mut())?;
    }
    publish(output, dest)
}

pub fn manifest_path(dir: &Path) -> (PathBuf, i64) {
    let path = dir.join(MANIFEST);
    if path.exists() {
        (path, FORMAT)
    } else {
        (dir.join("manifest.json"), LEGACY_FORMAT)
    }
}

pub fn read_committed(keys: &Path, dir: &Path, id: &str) -> Result<(String, Value)> {
    let (path, format) = manifest_path(dir);
    let bytes = std::fs::read(path).context("backup manifest")?;
    let digest = sha256_hex(&bytes);
    let complete = std::fs::read_to_string(dir.join("COMPLETE")).context("backup signature")?;
    let seal = verify_complete_version(keys, id, &digest, &complete, format)?;
    let bytes = if format == FORMAT {
        age_decrypt_bytes(keys, &bytes).context("decrypt backup manifest")?
    } else {
        bytes
    };
    let manifest: Value = serde_json::from_slice(&bytes).context("malformed backup manifest")?;
    if format == FORMAT {
        ensure!(
            manifest["format_version"] == FORMAT && manifest["backup_id"] == id,
            "backup manifest identity"
        );
        ensure!(
            Some(required_digest(&manifest, "seal_sha256")?) == seal,
            "backup plan digest mismatch"
        );
        required_digest(&manifest, "sqlite_sha256")?;
        required_digest(&manifest, "config_sha256")?;
        for item in items(&manifest)? {
            object_name(item)?;
            required_digest(item, "ciphertext_sha256")?;
        }
    }
    Ok((digest, manifest))
}

pub fn check_config(keys: &Path, dir: &Path, manifest: &Value) -> Result<Option<Vec<u8>>> {
    let Some(sha) = manifest["config_sha256"].as_str() else {
        return Ok(None);
    };
    let ciphertext = std::fs::read(dir.join(CONFIG))?;
    ensure!(
        sha256_hex(&ciphertext) == sha,
        "backup config hash mismatch"
    );
    let plain = age_decrypt_bytes(keys, &ciphertext)?;
    let config: Value = serde_json::from_slice(&plain)?;
    for key in [
        "instance_id",
        "schema_version",
        "routing_epoch",
        "branch_id",
        "parent_backup_id",
    ] {
        ensure!(
            config[key] == manifest[key],
            "backup config identity mismatch"
        );
    }
    Ok(Some(plain))
}

pub fn items(manifest: &Value) -> Result<&[Value]> {
    manifest["items"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("backup manifest items"))
}

pub fn object_name(item: &Value) -> Result<&str> {
    let name = required_digest(item, "object")?;
    let blob = item["blob_id"].as_str().unwrap_or(name);
    if blob != name {
        ensure!(
            name == sha256_hex(blob.as_bytes()),
            "backup object name mismatch"
        );
    }
    Ok(name)
}

fn required_digest<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let sha = value[key].as_str().context("missing backup digest")?;
    ensure!(
        sha.len() == 64 && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid backup digest"
    );
    Ok(sha)
}
