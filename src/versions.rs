use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, Transaction};

use crate::db;
use crate::ids::{new_id, sha256_hex};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VariantSpec {
    pub spec_id: String,
    pub fit: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub long_edge: Option<u32>,
    pub format: String,
    pub png_compression: String,
}

pub fn default_specs() -> Vec<VariantSpec> {
    vec![
        VariantSpec {
            spec_id: "thumb".into(),
            fit: "cover".into(),
            width: Some(256),
            height: Some(256),
            long_edge: None,
            format: "png".into(),
            png_compression: "fast".into(),
        },
        VariantSpec {
            spec_id: "mobile".into(),
            fit: "inside".into(),
            width: None,
            height: None,
            long_edge: Some(960),
            format: "png".into(),
            png_compression: "high".into(),
        },
        VariantSpec {
            spec_id: "tablet".into(),
            fit: "inside".into(),
            width: None,
            height: None,
            long_edge: Some(1440),
            format: "png".into(),
            png_compression: "medium".into(),
        },
        VariantSpec {
            spec_id: "desktop".into(),
            fit: "inside".into(),
            width: None,
            height: None,
            long_edge: Some(2048),
            format: "png".into(),
            png_compression: "default".into(),
        },
    ]
}

pub async fn ensure_default_profile(pool: &SqlitePool) -> Result<(String, i64, Vec<VariantSpec>)> {
    if let Some(row) = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT profile_id, revision, specs_json FROM variant_profiles
         ORDER BY revision DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    {
        return Ok((row.0, row.1, serde_json::from_str(&row.2)?));
    }
    let specs = default_specs();
    let json = serde_json::to_string(&specs)?;
    let hash = sha256_hex(json.as_bytes());
    sqlx::query(
        "INSERT INTO variant_profiles(profile_id, revision, specs_json, config_hash, created_at)
         VALUES('default', 1, ?, ?, ?)",
    )
    .bind(&json)
    .bind(&hash)
    .bind(db::now_ms())
    .execute(pool)
    .await?;
    Ok(("default".into(), 1, specs))
}

pub async fn current_profile(pool: &SqlitePool) -> Result<(i64, Vec<VariantSpec>)> {
    let (_id, rev, specs) = ensure_default_profile(pool).await?;
    Ok((rev, specs))
}

pub async fn plan_variants(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    image_id: &str,
    actor_id: Option<&str>,
    reservation_id: Option<&str>,
) -> Result<Vec<String>> {
    let (rev, specs) = {
        let row = sqlx::query_as::<_, (i64, String)>(
            "SELECT revision, specs_json FROM variant_profiles ORDER BY revision DESC LIMIT 1",
        )
        .fetch_one(&mut **tx)
        .await?;
        (row.0, serde_json::from_str::<Vec<VariantSpec>>(&row.1)?)
    };
    let mut jobs = Vec::new();
    for spec in specs {
        let variant_id = new_id();
        sqlx::query(
            "INSERT OR IGNORE INTO image_variants(id, image_id, profile_revision, spec_id, state)
             VALUES(?, ?, ?, ?, 'pending')",
        )
        .bind(&variant_id)
        .bind(image_id)
        .bind(rev)
        .bind(&spec.spec_id)
        .execute(&mut **tx)
        .await?;
        let priority = if spec.spec_id == "thumb" { 100 } else { 10 };
        let payload = crate::jobs::JobPayload {
            kind: "generate_variant".into(),
            payload_version: 1,
            scope: serde_json::json!({"image_id": image_id}),
            target: serde_json::json!({
                "profile_revision": rev,
                "spec_id": spec.spec_id
            }),
            actor_id: actor_id.map(|s| s.to_string()),
        };
        let job_id = if let Some(reservation_id) = reservation_id {
            crate::jobs::consume_reservation(tx, reservation_id, 1).await?;
            crate::jobs::enqueue_protected(
                tx,
                "generate_variant",
                &format!("generate_variant:{image_id}:{rev}:{}", spec.spec_id),
                &payload,
                actor_id,
                priority,
            )
            .await?
        } else {
            crate::jobs::enqueue(
                tx,
                "generate_variant",
                &format!("generate_variant:{image_id}:{rev}:{}", spec.spec_id),
                &payload,
                actor_id,
                priority,
            )
            .await?
        };
        jobs.push(job_id);
    }
    Ok(jobs)
}

pub async fn queue_missing_variants(tx: &mut Transaction<'_, sqlx::Sqlite>) -> Result<i64> {
    let Some(row) = sqlx::query_as::<_, (i64, String)>(
        "SELECT revision, specs_json FROM variant_profiles ORDER BY revision DESC LIMIT 1",
    )
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(0);
    };
    let rev = row.0;
    let specs = serde_json::from_str::<Vec<VariantSpec>>(&row.1)?;
    let now = db::now_ms();
    let mut queued = 0i64;
    for spec in specs {
        let images = sqlx::query_as::<_, (String,)>(
            "SELECT i.id FROM images i
             WHERE i.state IN ('active', 'trashed')
               AND NOT EXISTS (
                 SELECT 1 FROM variant_heads h
                 JOIN image_variants v ON v.id = h.variant_id
                 JOIN media_locations l ON l.blob_id = v.blob_id
                 JOIN storage_route r ON r.id = 1 AND l.backend_id = r.active_backend_id
                 WHERE h.image_id = i.id AND h.spec_id = ?
                   AND v.state = 'published' AND v.profile_revision = ?
                   AND l.state = 'verified'
               )
             ORDER BY i.id",
        )
        .bind(&spec.spec_id)
        .bind(rev)
        .fetch_all(&mut **tx)
        .await?;
        for (image_id,) in images {
            sqlx::query(
                "INSERT OR IGNORE INTO image_variants(id, image_id, profile_revision, spec_id, state)
                 VALUES(?, ?, ?, ?, 'pending')",
            )
            .bind(new_id())
            .bind(&image_id)
            .bind(rev)
            .bind(&spec.spec_id)
            .execute(&mut **tx)
            .await?;
            let dedupe = format!("generate_variant:{image_id}:{rev}:{}", spec.spec_id);
            let reopened = sqlx::query(
                "UPDATE jobs SET state = 'queued', failure_code = NULL, lease_token = NULL, lease_until = NULL,
                    available_at = ?, updated_at = ?, attempt_no = 0
                 WHERE dedupe_key = ? AND state IN ('succeeded', 'failed', 'canceled')",
            )
            .bind(now)
            .bind(now)
            .bind(&dedupe)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if reopened == 0 {
                let existing: Option<(String,)> =
                    sqlx::query_as("SELECT id FROM jobs WHERE dedupe_key = ?")
                        .bind(&dedupe)
                        .fetch_optional(&mut **tx)
                        .await?;
                if existing.is_none() {
                    let payload = crate::jobs::JobPayload {
                        kind: "generate_variant".into(),
                        payload_version: 1,
                        scope: serde_json::json!({"image_id": image_id}),
                        target: serde_json::json!({
                            "profile_revision": rev,
                            "spec_id": spec.spec_id
                        }),
                        actor_id: None,
                    };
                    let priority = if spec.spec_id == "thumb" { 100 } else { 10 };
                    crate::jobs::enqueue_protected(
                        tx,
                        "generate_variant",
                        &dedupe,
                        &payload,
                        None,
                        priority,
                    )
                    .await?;
                }
            }
            queued += 1;
        }
    }
    Ok(queued)
}

pub async fn publication_stats(pool: &SqlitePool) -> Result<serde_json::Value> {
    let (revision, specs) = current_profile(pool).await?;
    let spec_ids: Vec<String> = specs.iter().map(|spec| spec.spec_id.clone()).collect();
    let spec_count = spec_ids.len() as i64;
    let (active,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images WHERE state = 'active'")
        .fetch_one(pool)
        .await?;
    let (published,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM variant_heads h
         JOIN image_variants v ON v.id = h.variant_id
         JOIN images i ON i.id = h.image_id
         WHERE i.state = 'active' AND v.state = 'published'
           AND h.spec_id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(&spec_ids)?)
    .fetch_one(pool)
    .await?;
    let (preview_missing,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM images i
         WHERE i.state = 'active'
           AND NOT EXISTS (
                SELECT 1 FROM variant_heads h
                JOIN image_variants v ON v.id = h.variant_id
                WHERE h.image_id = i.id AND h.spec_id = 'thumb' AND v.state = 'published'
           )",
    )
    .fetch_one(pool)
    .await?;
    let (backlog,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs
         WHERE kind = 'generate_variant' AND state IN ('queued', 'running', 'retry_wait')",
    )
    .fetch_one(pool)
    .await?;
    let expected = active * spec_count;
    let published_ratio = if expected == 0 { 0.0 } else { published as f64 / expected as f64 };
    let preview_missing_rate = if active == 0 { 0.0 } else { preview_missing as f64 / active as f64 };
    Ok(serde_json::json!({
        "profile_revision": revision,
        "active_images": active,
        "spec_count": spec_count,
        "published": published,
        "published_ratio": published_ratio,
        "backlog": backlog,
        "preview_missing": preview_missing,
        "preview_missing_rate": preview_missing_rate
    }))
}

pub async fn published_variants(
    pool: &SqlitePool,
    image_id: &str,
) -> Result<Vec<PublishedVariant>> {
    let rows = sqlx::query_as::<_, (String, String, Option<i64>, Option<i64>, i64)>(
        "SELECT v.id, v.spec_id, v.width, v.height, v.profile_revision
         FROM variant_heads h
         JOIN image_variants v ON v.id = h.variant_id
         WHERE h.image_id = ? AND v.state = 'published'",
    )
    .bind(image_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, spec_id, width, height, rev)| PublishedVariant {
            id,
            spec_id,
            width,
            height,
            profile_revision: rev,
        })
        .collect())
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishedVariant {
    pub id: String,
    pub spec_id: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub profile_revision: i64,
}

pub async fn reclaim_expired_variants(
    pool: &SqlitePool,
    media: &crate::storage::Storage,
) -> Result<i64> {
    let now = crate::db::now_ms();
    let due = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT v.id, v.blob_id FROM image_variants v
         WHERE v.retain_until IS NOT NULL AND v.retain_until <= ? AND v.state = 'published'
           AND NOT EXISTS (SELECT 1 FROM variant_heads h WHERE h.variant_id = v.id)",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    let mut removed = 0_i64;
    for (variant_id, blob_id) in due {
        if let Some(blob_id) = blob_id {
            let (shared,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM image_variants
                 WHERE blob_id = ? AND id != ? AND state = 'published'",
            )
            .bind(&blob_id)
            .bind(&variant_id)
            .fetch_one(pool)
            .await?;
            if shared == 0 {
                if let Ok((key, _, _, _)) =
                    crate::storage::location_for(pool, &blob_id, &media.backend_id).await
                {
                    let _ = media.delete_key(&key).await;
                }
            }
        }
        sqlx::query(
            "UPDATE image_variants SET state = 'superseded'
             WHERE id = ? AND state = 'published'
               AND NOT EXISTS (SELECT 1 FROM variant_heads h WHERE h.variant_id = image_variants.id)",
        )
        .bind(&variant_id)
        .execute(pool)
        .await?;
        removed += 1;
    }
    Ok(removed)
}

pub async fn claim_variant_head(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    image_id: &str,
    spec_id: &str,
    variant_id: &str,
    profile_revision: i64,
) -> Result<bool> {
    let current = sqlx::query_as::<_, (i64,)>(
        "SELECT v.profile_revision FROM variant_heads h
         JOIN image_variants v ON v.id = h.variant_id
         WHERE h.image_id = ? AND h.spec_id = ?",
    )
    .bind(image_id)
    .bind(spec_id)
    .fetch_optional(&mut **tx)
    .await?;
    if current.is_some_and(|row| row.0 > profile_revision) {
        return Ok(false);
    }
    let now = crate::db::now_ms();
    sqlx::query(
        "INSERT INTO variant_heads(image_id, spec_id, variant_id, published_at)
         VALUES(?, ?, ?, ?)
         ON CONFLICT(image_id, spec_id) DO UPDATE SET
            variant_id = excluded.variant_id,
            published_at = excluded.published_at",
    )
    .bind(image_id)
    .bind(spec_id)
    .bind(variant_id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

pub fn detail_srcset(variants: &[PublishedVariant]) -> Vec<PublishedVariant> {
    let rank = |spec: &str| match spec {
        "mobile" => 0,
        "tablet" => 1,
        "desktop" => 2,
        _ => 9,
    };
    let mut rows: Vec<PublishedVariant> = variants
        .iter()
        .filter(|variant| variant.spec_id != "thumb" && variant.width.unwrap_or(0) > 0)
        .cloned()
        .collect();
    rows.sort_by(|left, right| {
        left.width
            .unwrap_or(0)
            .cmp(&right.width.unwrap_or(0))
            .then_with(|| rank(&left.spec_id).cmp(&rank(&right.spec_id)))
    });
    let mut seen = std::collections::HashSet::new();
    rows.retain(|variant| seen.insert(variant.width.unwrap_or(0)));
    rows
}

pub fn select_for_spec<'a>(
    variants: &'a [PublishedVariant],
    spec_id: &str,
) -> Option<(&'a PublishedVariant, bool)> {
    if let Some(exact) = variants.iter().find(|variant| variant.spec_id == spec_id) {
        return Some((exact, false));
    }
    let target = match spec_id {
        "thumb" => 256,
        "mobile" => 960,
        "tablet" => 1440,
        "desktop" => 2048,
        _ => 256,
    };
    choose_fallback(variants, target).map(|variant| (variant, true))
}

pub fn choose_fallback<'a>(
    variants: &'a [PublishedVariant],
    target_long: u32,
) -> Option<&'a PublishedVariant> {
    let mut larger: Vec<_> = variants
        .iter()
        .filter(|v| v.width.unwrap_or(0).max(v.height.unwrap_or(0)) as u32 >= target_long)
        .collect();
    larger.sort_by_key(|v| v.width.unwrap_or(0).max(v.height.unwrap_or(0)));
    if let Some(v) = larger.first() {
        return Some(*v);
    }
    variants
        .iter()
        .max_by_key(|v| v.width.unwrap_or(0).max(v.height.unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(id: &str, spec: &str, long: i64) -> PublishedVariant {
        PublishedVariant {
            id: id.into(),
            spec_id: spec.into(),
            width: Some(long),
            height: Some(long / 2),
            profile_revision: 1,
        }
    }

    #[test]
    fn missing_spec_uses_the_smallest_sufficient_derivative() {
        let variants = vec![variant("t", "thumb", 256), variant("m", "mobile", 960), variant("tab", "tablet", 1440)];
        let (desktop, fallback) = select_for_spec(&variants, "desktop").unwrap();
        assert!(fallback);
        assert_eq!(desktop.spec_id, "tablet");
        let (mobile, fallback) = select_for_spec(&variants, "mobile").unwrap();
        assert!(!fallback);
        assert_eq!(mobile.id, "m");
        let only_thumb = [variant("t", "thumb", 256)];
        let (only, fallback) = select_for_spec(&only_thumb, "desktop").unwrap();
        assert!(fallback);
        assert_eq!(only.spec_id, "thumb");
        assert!(select_for_spec(&[], "thumb").is_none());
    }

    #[test]
    fn same_width_detail_candidates_keep_one() {
        let variants = vec![
            variant("t", "thumb", 256),
            variant("m", "mobile", 80),
            variant("tab", "tablet", 80),
            variant("d", "desktop", 80),
        ];
        let srcset = detail_srcset(&variants);
        assert_eq!(srcset.len(), 1);
        assert_eq!(srcset[0].spec_id, "mobile");
        assert!(detail_srcset(&[variant("t", "thumb", 256)]).is_empty());
    }
}
