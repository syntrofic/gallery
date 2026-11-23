use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    db,
    error::{AppError, AppResult},
    versions,
};

const PROFILE_KEY: &str = "gallery_presentation";

#[derive(Debug, Serialize, Deserialize)]
pub struct Profile {
    pub title: String,
    pub description: String,
    pub revision: i64,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            title: "Gallery".into(),
            description: String::new(),
            revision: 0,
        }
    }
}

pub async fn profile(pool: &SqlitePool) -> AppResult<Profile> {
    let raw = db::kv_get(pool, PROFILE_KEY).await?;
    read_profile(raw.as_deref())
}

fn read_profile(raw: Option<&str>) -> AppResult<Profile> {
    raw.map_or_else(
        || Ok(Profile::default()),
        |value| serde_json::from_str(value).map_err(|err| anyhow::Error::from(err).into()),
    )
}

pub async fn write_profile(
    pool: &SqlitePool,
    title: &str,
    description: &str,
    expected_revision: i64,
) -> AppResult<Profile> {
    let title = title.trim();
    let description = description.trim();
    if title.is_empty() || title.chars().count() > 80 || description.chars().count() > 400 {
        return Err(AppError::unprocessable(
            "GALLERY_PRESENTATION",
            "invalid gallery title or description",
        ));
    }
    let mut tx = db::begin_immediate(pool).await?;
    let saved: Option<String> = sqlx::query_scalar("SELECT value FROM schema_app WHERE key = ?")
        .bind(PROFILE_KEY)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let current = read_profile(saved.as_deref())?;
    if current.revision != expected_revision {
        return Err(AppError::precondition(
            "REVISION_CONFLICT",
            "gallery presentation revision",
        ));
    }
    let next = Profile {
        title: title.into(),
        description: description.into(),
        revision: current.revision + 1,
    };
    db::kv_set(
        &mut tx,
        PROFILE_KEY,
        &serde_json::to_string(&next).map_err(anyhow::Error::from)?,
    )
    .await?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(next)
}

#[derive(Serialize)]
pub struct Work {
    pub id: String,
    pub album_id: String,
    pub display_name: String,
    pub width: i64,
    pub height: i64,
    pub variants: Vec<versions::PublishedVariant>,
}

type WorkRow = (String, String, String, i64, i64, i64);

async fn work(pool: &SqlitePool, row: WorkRow) -> AppResult<Work> {
    let variants = versions::published_variants(pool, &row.0).await?;
    Ok(Work {
        id: row.0,
        album_id: row.1,
        display_name: row.2,
        width: row.3,
        height: row.4,
        variants,
    })
}

#[derive(Serialize)]
pub struct WorkPage {
    pub items: Vec<Work>,
    pub next_cursor: Option<String>,
}

pub async fn works(
    pool: &SqlitePool,
    album: Option<&str>,
    limit: i64,
    cursor: Option<&str>,
) -> AppResult<WorkPage> {
    let limit = limit.clamp(1, 100);
    let cursor = cursor.map(decode_cursor).transpose()?;
    let rows: Vec<WorkRow> = sqlx::query_as(
        "WITH public_works AS (
            SELECT i.id, MIN(a.id) AS album_id, i.original_name, i.width, i.height, i.created_at
            FROM albums a JOIN album_images ai ON ai.album_id = a.id
            JOIN images i ON i.id = ai.image_id
            WHERE a.visibility = 'public' AND a.deleted_at IS NULL AND i.state = 'active'
              AND (? IS NULL OR a.id = ?)
            GROUP BY i.id
         )
         SELECT work.id, work.album_id, COALESCE(member.display_name, work.original_name),
                work.width, work.height, work.created_at
         FROM public_works work JOIN album_images member
           ON member.album_id = work.album_id AND member.image_id = work.id
         WHERE (? IS NULL OR (work.created_at, work.id) < (?, ?))
         ORDER BY work.created_at DESC, work.id DESC LIMIT ?",
    )
    .bind(album)
    .bind(album)
    .bind(cursor.as_ref().map(|row| row.0))
    .bind(cursor.as_ref().map(|row| row.0))
    .bind(cursor.as_ref().map(|row| row.1.as_str()))
    .bind(limit + 1)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let more = rows.len() > limit as usize;
    let mut items = Vec::new();
    let mut next_cursor = None;
    for row in rows.into_iter().take(limit as usize) {
        next_cursor = more.then(|| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{}:{}", row.5, row.0))
        });
        items.push(work(pool, row).await?);
    }
    Ok(WorkPage { items, next_cursor })
}

fn decode_cursor(raw: &str) -> AppResult<(i64, String)> {
    let parsed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|value| {
            let (at, id) = value.split_once(':')?;
            if id.is_empty() {
                return None;
            }
            Some((at.parse().ok()?, id.to_owned()))
        });
    parsed.ok_or_else(|| AppError::bad_request("PORTFOLIO_CURSOR", "invalid gallery cursor"))
}

#[derive(Serialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    pub description: String,
    pub count: i64,
    pub covers: Vec<Work>,
}

pub async fn collections(pool: &SqlitePool) -> AppResult<Vec<Collection>> {
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT a.id, a.name, a.description, COUNT(i.id)
         FROM albums a LEFT JOIN album_images ai ON ai.album_id = a.id
         LEFT JOIN images i ON i.id = ai.image_id AND i.state = 'active'
         WHERE a.visibility = 'public' AND a.deleted_at IS NULL
         GROUP BY a.id ORDER BY a.created_at DESC, a.id DESC LIMIT 100",
    )
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut collections = Vec::with_capacity(rows.len());
    for (id, name, description, count) in rows {
        let candidates: Vec<WorkRow> = sqlx::query_as(
            "SELECT i.id, a.id, COALESCE(ai.display_name, i.original_name), i.width, i.height, i.created_at
             FROM albums a JOIN album_images ai ON a.id = ai.album_id
             JOIN images i ON i.id = ai.image_id
             WHERE a.id = ? AND a.visibility = 'public' AND a.deleted_at IS NULL AND i.state = 'active'
             ORDER BY CASE WHEN i.id = a.cover_image_id THEN 0 ELSE 1 END, ai.sort_key, i.id LIMIT 3",
        ).bind(&id).fetch_all(pool).await.map_err(anyhow::Error::from)?;
        let mut covers = Vec::new();
        for candidate in candidates {
            covers.push(work(pool, candidate).await?);
        }
        collections.push(Collection {
            id,
            name,
            description,
            count,
            covers,
        });
    }
    Ok(collections)
}
