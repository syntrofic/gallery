use base64::Engine;
use serde_json::{json, Value};
use sqlx::{Sqlite, SqlitePool};

use crate::auth::Actor;
use crate::error::{AppError, AppResult};

struct Doc {
    kind: String,
    id: String,
    album_id: Option<String>,
    name: String,
    description: String,
    body: String,
}

pub async fn query(
    pool: &SqlitePool,
    actor: Option<&Actor>,
    term: &str,
    kind: Option<&str>,
    album_id: Option<&str>,
    limit: i64,
    cursor: Option<&str>,
) -> AppResult<Value> {
    if let Some(kind) = kind {
        if kind != "album" && kind != "image" {
            return Err(AppError::unprocessable("SEARCH_KIND", "kind must be album or image"));
        }
    }
    let term = normalize(term);
    let limit = limit.clamp(1, 200);
    if term.is_empty() {
        return Ok(json!({"items": {"albums": [], "images": []}, "next_cursor": null}));
    }
    let anchor = cursor.map(decode_cursor).transpose()?.flatten();
    let mut docs = load_docs(pool, actor, kind, album_id).await?;
    if term.chars().count() >= 3 {
        docs = match indexed_keys(pool, &term).await {
            Some(keys) => docs.into_iter().filter(|doc| keys.contains(&doc.key())).collect(),
            None => match_fts(pool, docs, &term).await?,
        };
    } else {
        docs.retain(|doc| doc.body.contains(&term));
    }
    docs.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.id.cmp(&b.id))
            .then(a.album_id.cmp(&b.album_id))
    });
    let start = if let Some(anchor) = anchor {
        docs.iter()
            .position(|doc| doc.key() == anchor)
            .ok_or_else(|| AppError::conflict("CURSOR_STALE", "cursor does not match this search"))?
            + 1
    } else {
        0
    };
    let end = (start + limit as usize).min(docs.len());
    let page = &docs[start..end];
    let next_cursor = if end < docs.len() {
        page.last().map(|doc| encode_cursor(&doc.key()))
    } else {
        None
    };
    let albums: Vec<_> = page
        .iter()
        .filter(|doc| doc.kind == "album")
        .map(|doc| json!({"id": doc.id, "name": doc.name, "description": doc.description}))
        .collect();
    let images: Vec<_> = page
        .iter()
        .filter(|doc| doc.kind == "image")
        .map(|doc| {
            json!({
                "id": doc.id,
                "display_name": doc.name,
                "album_id": doc.album_id
            })
        })
        .collect();
    Ok(json!({
        "items": {"albums": albums, "images": images},
        "next_cursor": next_cursor
    }))
}

fn normalize(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

impl Doc {
    fn key(&self) -> String {
        format!(
            "{}\t{}\t{}",
            self.kind,
            self.id,
            self.album_id.as_deref().unwrap_or("")
        )
    }
}

async fn load_docs(
    pool: &SqlitePool,
    actor: Option<&Actor>,
    kind: Option<&str>,
    album_filter: Option<&str>,
) -> AppResult<Vec<Doc>> {
    let mut docs = Vec::new();
    if kind != Some("image") {
        let albums = visible_albums(pool, actor, album_filter).await?;
        docs.extend(albums);
    }
    if kind != Some("album") {
        let images = visible_images(pool, actor, album_filter).await?;
        docs.extend(images);
        if album_filter.is_none() && actor.is_some_and(|actor| actor.role == "owner") {
            docs.extend(unassigned_images(pool).await?);
        }
    }
    Ok(docs)
}

async fn visible_albums(
    pool: &SqlitePool,
    actor: Option<&Actor>,
    album_filter: Option<&str>,
) -> AppResult<Vec<Doc>> {
    let (sql, bind_actor) = match actor {
        Some(actor) if actor.role == "owner" => (
            "SELECT id, name, description FROM albums WHERE deleted_at IS NULL AND (? IS NULL OR id = ?) ORDER BY id",
            None,
        ),
        Some(actor) => (
            "SELECT a.id, a.name, a.description FROM albums a
             WHERE a.deleted_at IS NULL AND (? IS NULL OR a.id = ?)
               AND (a.visibility = 'public' OR EXISTS (
                    SELECT 1 FROM album_grants g
                    WHERE g.album_id = a.id AND g.actor_id = ? AND g.browse = 1))
             ORDER BY a.id",
            Some(actor.id.as_str()),
        ),
        None => (
            "SELECT id, name, description FROM albums
             WHERE deleted_at IS NULL AND visibility = 'public' AND (? IS NULL OR id = ?)
             ORDER BY id",
            None,
        ),
    };
    let mut query = sqlx::query_as::<_, (String, String, String)>(sql).bind(album_filter).bind(album_filter);
    if let Some(actor_id) = bind_actor {
        query = query.bind(actor_id);
    }
    let rows = query.fetch_all(pool).await.map_err(anyhow::Error::from)?;
    Ok(rows
        .into_iter()
        .map(|(id, name, description)| Doc {
            body: normalize(&format!("{name} {description}")),
            kind: "album".into(),
            id,
            album_id: None,
            name,
            description,
        })
        .collect())
}

async fn visible_images(
    pool: &SqlitePool,
    actor: Option<&Actor>,
    album_filter: Option<&str>,
) -> AppResult<Vec<Doc>> {
    let (sql, bind_actor) = match actor {
        Some(actor) if actor.role == "owner" => (
            "SELECT i.id, COALESCE(ai.display_name, i.original_name), a.id,
                    (SELECT c.text FROM captions c WHERE c.image_id = i.id
                     ORDER BY CASE c.source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1)
             FROM images i
             JOIN album_images ai ON ai.image_id = i.id
             JOIN albums a ON a.id = ai.album_id
             WHERE i.state = 'active' AND a.deleted_at IS NULL AND (? IS NULL OR a.id = ?)",
            None,
        ),
        Some(actor) => (
            "SELECT i.id, COALESCE(ai.display_name, i.original_name), a.id,
                    (SELECT c.text FROM captions c WHERE c.image_id = i.id
                     ORDER BY CASE c.source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1)
             FROM images i
             JOIN album_images ai ON ai.image_id = i.id
             JOIN albums a ON a.id = ai.album_id
             WHERE i.state = 'active' AND a.deleted_at IS NULL AND (? IS NULL OR a.id = ?)
               AND (a.visibility = 'public' OR EXISTS (
                    SELECT 1 FROM album_grants g
                    WHERE g.album_id = a.id AND g.actor_id = ? AND g.browse = 1))",
            Some(actor.id.as_str()),
        ),
        None => (
            "SELECT i.id, COALESCE(ai.display_name, i.original_name), a.id,
                    (SELECT c.text FROM captions c WHERE c.image_id = i.id
                     ORDER BY CASE c.source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1)
             FROM images i
             JOIN album_images ai ON ai.image_id = i.id
             JOIN albums a ON a.id = ai.album_id
             WHERE i.state = 'active' AND a.deleted_at IS NULL AND a.visibility = 'public'
               AND (? IS NULL OR a.id = ?)",
            None,
        ),
    };
    let mut query = sqlx::query_as::<_, (String, String, String, Option<String>)>(sql)
        .bind(album_filter)
        .bind(album_filter);
    if let Some(actor_id) = bind_actor {
        query = query.bind(actor_id);
    }
    let rows = query.fetch_all(pool).await.map_err(anyhow::Error::from)?;
    Ok(rows
        .into_iter()
        .map(|(id, name, album_id, caption)| Doc {
            body: normalize(&format!("{name} {}", caption.unwrap_or_default())),
            kind: "image".into(),
            id,
            album_id: Some(album_id),
            name,
            description: String::new(),
        })
        .collect())
}

async fn unassigned_images(pool: &SqlitePool) -> AppResult<Vec<Doc>> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT i.id, i.original_name,
                (SELECT c.text FROM captions c WHERE c.image_id = i.id
                 ORDER BY CASE c.source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1)
         FROM images i
         WHERE i.state = 'active' AND NOT EXISTS (
            SELECT 1 FROM album_images ai
            JOIN albums a ON a.id = ai.album_id
            WHERE ai.image_id = i.id AND a.deleted_at IS NULL)",
    )
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(rows
        .into_iter()
        .map(|(id, name, caption)| Doc {
            body: normalize(&format!("{name} {}", caption.unwrap_or_default())),
            kind: "image".into(),
            id,
            album_id: None,
            name,
            description: String::new(),
        })
        .collect())
}

pub async fn sync_album(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    album_id: &str,
) -> anyhow::Result<()> {
    if !index_ready(tx).await? {
        return Ok(());
    }
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT name, description FROM albums WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(album_id)
    .fetch_optional(&mut **tx)
    .await?;
    let docs = row
        .map(|(name, description)| {
            vec![Doc {
                body: normalize(&format!("{name} {description}")),
                kind: "album".into(),
                id: album_id.to_string(),
                album_id: None,
                name,
                description,
            }]
        })
        .unwrap_or_default();
    replace_docs(tx, "album", album_id, &docs).await
}

pub async fn sync_image(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    image_id: &str,
) -> anyhow::Result<()> {
    if !index_ready(tx).await? {
        return Ok(());
    }
    let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT COALESCE(ai.display_name, i.original_name), a.id,
                (SELECT c.text FROM captions c WHERE c.image_id = i.id
                 ORDER BY CASE c.source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1)
         FROM images i
         JOIN album_images ai ON ai.image_id = i.id
         JOIN albums a ON a.id = ai.album_id
         WHERE i.id = ? AND i.state = 'active' AND a.deleted_at IS NULL",
    )
    .bind(image_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut docs: Vec<Doc> = rows
        .into_iter()
        .map(|(name, album_id, caption)| Doc {
            body: normalize(&format!("{name} {}", caption.unwrap_or_default())),
            kind: "image".into(),
            id: image_id.to_string(),
            album_id: Some(album_id),
            name,
            description: String::new(),
        })
        .collect();
    if docs.is_empty() {
        let lone = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT i.original_name,
                    (SELECT c.text FROM captions c WHERE c.image_id = i.id
                     ORDER BY CASE c.source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1)
             FROM images i
             WHERE i.id = ? AND i.state = 'active'",
        )
        .bind(image_id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((name, caption)) = lone {
            docs.push(Doc {
                body: normalize(&format!("{name} {}", caption.unwrap_or_default())),
                kind: "image".into(),
                id: image_id.to_string(),
                album_id: None,
                name,
                description: String::new(),
            });
        }
    }
    replace_docs(tx, "image", image_id, &docs).await
}

async fn index_ready(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> anyhow::Result<bool> {
    let row = sqlx::query_as::<_, (String,)>("SELECT value FROM schema_app WHERE key = 'search_index_ready'")
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.as_ref().map(|value| value.0.as_str()) == Some("1"))
}

async fn replace_docs(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    resource_id: &str,
    docs: &[Doc],
) -> anyhow::Result<()> {
    let old = sqlx::query_as::<_, (String,)>(
        "SELECT doc_key FROM search_documents WHERE kind = ? AND resource_id = ?",
    )
    .bind(kind)
    .bind(resource_id)
    .fetch_all(&mut **tx)
    .await?;
    for (key,) in old {
        let rowids = sqlx::query_as::<_, (i64,)>("SELECT rowid FROM search_fts WHERE doc_key = ?")
            .bind(&key)
            .fetch_all(&mut **tx)
            .await?;
        for (rowid,) in rowids {
            sqlx::query("DELETE FROM search_fts WHERE rowid = ?")
                .bind(rowid)
                .execute(&mut **tx)
                .await?;
        }
    }
    sqlx::query("DELETE FROM search_documents WHERE kind = ? AND resource_id = ?")
        .bind(kind)
        .bind(resource_id)
        .execute(&mut **tx)
        .await?;
    for doc in docs {
        sqlx::query(
            "INSERT INTO search_documents(doc_key, kind, resource_id, album_id, name, description, body)
             VALUES(?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(doc.key())
        .bind(&doc.kind)
        .bind(&doc.id)
        .bind(&doc.album_id)
        .bind(&doc.name)
        .bind(&doc.description)
        .bind(&doc.body)
        .execute(&mut **tx)
        .await?;
        sqlx::query("INSERT INTO search_fts(doc_key, body) VALUES(?, ?)")
            .bind(doc.key())
            .bind(&doc.body)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub async fn rebuild(pool: &SqlitePool) -> anyhow::Result<i64> {
    let owner = Actor {
        id: "search-rebuild".into(),
        username: "search-rebuild".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let docs = load_docs(pool, Some(&owner), None, None)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let mut tx = crate::db::begin_immediate(pool).await?;
    sqlx::query("DELETE FROM search_documents").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM search_fts").execute(&mut *tx).await?;
    for doc in &docs {
        sqlx::query(
            "INSERT INTO search_documents(doc_key, kind, resource_id, album_id, name, description, body)
             VALUES(?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(doc.key())
        .bind(&doc.kind)
        .bind(&doc.id)
        .bind(&doc.album_id)
        .bind(&doc.name)
        .bind(&doc.description)
        .bind(&doc.body)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO search_fts(doc_key, body) VALUES(?, ?)")
            .bind(doc.key())
            .bind(&doc.body)
            .execute(&mut *tx)
            .await?;
    }
    crate::db::kv_set(&mut tx, "search_index_ready", "1").await?;
    crate::db::kv_set(&mut tx, "search_index_count", &docs.len().to_string()).await?;
    tx.commit().await?;
    Ok(docs.len() as i64)
}

async fn indexed_keys(pool: &SqlitePool, term: &str) -> Option<std::collections::HashSet<String>> {
    if crate::db::kv_get(pool, "search_index_ready").await.ok().flatten().as_deref() != Some("1") {
        return None;
    }
    let quoted = term.replace('"', "\"\"");
    let hits = sqlx::query_as::<Sqlite, (String,)>("SELECT doc_key FROM search_fts WHERE search_fts MATCH ?")
        .bind(format!("\"{quoted}\""))
        .fetch_all(pool)
        .await
        .ok()?;
    Some(hits.into_iter().map(|row| row.0).collect())
}

async fn match_fts(pool: &SqlitePool, docs: Vec<Doc>, term: &str) -> AppResult<Vec<Doc>> {
    if docs.is_empty() {
        return Ok(docs);
    }
    let mut conn = pool.acquire().await.map_err(anyhow::Error::from)?;
    sqlx::query("DROP TABLE IF EXISTS temp.search_docs")
        .execute(&mut *conn)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "CREATE VIRTUAL TABLE temp.search_docs USING fts5(
            doc_key UNINDEXED,
            body,
            tokenize = 'trigram'
         )",
    )
    .execute(&mut *conn)
    .await
    .map_err(anyhow::Error::from)?;
    for doc in &docs {
        sqlx::query("INSERT INTO temp.search_docs(doc_key, body) VALUES(?, ?)")
            .bind(doc.key())
            .bind(&doc.body)
            .execute(&mut *conn)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let quoted = term.replace('"', "\"\"");
    let hits = sqlx::query_as::<Sqlite, (String,)>(
        "SELECT doc_key FROM temp.search_docs WHERE search_docs MATCH ?",
    )
    .bind(format!("\"{quoted}\""))
    .fetch_all(&mut *conn)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("DROP TABLE IF EXISTS temp.search_docs")
        .execute(&mut *conn)
        .await
        .map_err(anyhow::Error::from)?;
    let keys: std::collections::HashSet<String> = hits.into_iter().map(|row| row.0).collect();
    Ok(docs.into_iter().filter(|doc| keys.contains(&doc.key())).collect())
}

fn encode_cursor(key: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.as_bytes())
}

fn decode_cursor(raw: &str) -> AppResult<Option<String>> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, raw)
        .map_err(|_| AppError::conflict("CURSOR_STALE", "cursor does not match this search"))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| AppError::conflict("CURSOR_STALE", "cursor does not match this search"))?;
    let mut parts = text.split('\t');
    let kind = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    if (kind != "album" && kind != "image") || id.is_empty() || parts.next().is_none() {
        return Err(AppError::conflict("CURSOR_STALE", "cursor does not match this search"));
    }
    Ok(Some(text))
}
