use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use reqwest::{header, Client, Url};
use serde_json::{json, Value};

use crate::{auth::Actor, backup, db};

pub async fn switch_caddy(
    config: &crate::config::Config,
    source: &str,
    target: &str,
    source_ack: &str,
    caddy_admin: &str,
    config_path: &str,
) -> Result<Value> {
    let source = origin(source)?;
    let target = origin(target)?;
    let admin = origin(caddy_admin)?;
    ensure!(
        source != target && source.scheme() == target.scheme(),
        "upstreams must be distinct origins using the same scheme"
    );
    ensure!(
        admin.scheme() == "http"
            && admin
                .host_str()
                .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]")),
        "Caddy admin must be local"
    );
    ensure!(
        config_path.starts_with("/config/apps/http/servers/")
            && config_path.split('/').skip(1).all(|part| !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))),
        "expected the full Caddy reverse-proxy handler configuration path"
    );

    let pool = db::connect(&config.db_path()).await?;
    let instance = db::instance_id(&pool).await?;
    let accepted_at = db::kv_get(&pool, "restore_accepted_at")
        .await?
        .context("the local restore needs to be accepted")?
        .parse::<i64>()?;
    let actor: (String, String, i64) = sqlx::query_as(
        "SELECT id, username, revision FROM actors WHERE role = 'owner' AND status = 'active'",
    )
    .fetch_one(&pool)
    .await?;
    let actor = Actor {
        id: actor.0,
        username: actor.1,
        revision: actor.2,
        role: "owner".into(),
        must_change_password: false,
    };

    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    for upstream in [&source, &target] {
        client
            .get(upstream.join("/health/ready")?)
            .send()
            .await?
            .error_for_status()?;
    }
    let old = read_entry(&client, &source).await?;
    let new = read_entry(&client, &target).await?;
    let frozen_at = old["frozen_at"]
        .as_i64()
        .context("the old host has not frozen writes")?;
    let expected = crate::ids::sha256_hex(format!("{instance}:{frozen_at}").as_bytes());
    ensure!(
        old["instance_id"] == instance && old["writes"] == "blocked" && expected == source_ack,
        "source freeze acknowledgement does not match"
    );
    ensure!(
        new["instance_id"] == instance
            && new["writes"] == "open"
            && new["accepted_at"] == accepted_at,
        "target is not the locally accepted restore"
    );

    let url = admin.join(config_path)?;
    let response = client.get(url.clone()).send().await?.error_for_status()?;
    let etag = response
        .headers()
        .get(header::ETAG)
        .context("Caddy config has no ETag")?
        .clone();
    let mut proxy: Value = serde_json::from_slice(&response.bytes().await?)?;
    ensure!(
        proxy["handler"] == "reverse_proxy" && proxy["dynamic_upstreams"].is_null(),
        "expected a static reverse proxy handler"
    );
    ensure!(
        !proxy["transport"]["tls"].is_null() == (source.scheme() == "https"),
        "Caddy upstream TLS must match both origin schemes"
    );
    let upstreams = proxy["upstreams"]
        .as_array_mut()
        .context("Caddy upstreams missing")?;
    ensure!(upstreams.len() == 1, "expected one Caddy upstream");
    let source_dial = dial(&source)?;
    let target_dial = dial(&target)?;
    let current = upstreams[0]["dial"]
        .as_str()
        .context("Caddy upstream dial address missing")?;
    ensure!(
        current == source_dial || current == target_dial,
        "Caddy upstream differs from both approved hosts"
    );
    let switched = current != target_dial;
    let completed = backup::finish_handoff(&pool, &actor, source_ack).await?;
    if switched {
        upstreams[0]["dial"] = json!(target_dial);
        let result = client
            .patch(url)
            .header(header::IF_MATCH, etag)
            .header(header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&proxy)?)
            .send()
            .await?;
        ensure!(
            result.status().is_success(),
            "Caddy cutover rejected with HTTP {}",
            result.status()
        );
    }
    Ok(json!({
        "backup_id": completed["backup_id"],
        "source": source_dial,
        "target": target_dial,
        "proxy_changed": switched,
    }))
}

fn origin(raw: &str) -> Result<Url> {
    let url = Url::parse(raw)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "expected an HTTP(S) origin"
    );
    Ok(url)
}

fn dial(url: &Url) -> Result<String> {
    Ok(format!(
        "{}:{}",
        url.host_str().context("missing host")?,
        url.port_or_known_default().context("missing port")?
    ))
}

async fn read_entry(client: &Client, url: &Url) -> Result<Value> {
    let response = client
        .get(url.join("/api/v1/library/entry")?)
        .send()
        .await?
        .error_for_status()?;
    serde_json::from_slice(&response.bytes().await?)
        .map_err(|_| anyhow!("invalid Gallery handoff response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use std::sync::{Arc, Mutex};

    async fn serve(router: Router) -> Result<Url> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = Url::parse(&format!("http://{}", listener.local_addr()?))?;
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Ok(origin)
    }

    #[tokio::test]
    async fn caddy_changes_only_the_approved_upstream_with_a_precondition() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut cfg = crate::config::Config::from_env()?;
        cfg.data_dir = temp.path().to_path_buf();
        let pool = db::connect(&cfg.db_path()).await?;
        crate::storage::bootstrap_local(&pool, cfg.media_root()).await?;
        let now = db::now_ms();
        let mut tx = db::begin_immediate(&pool).await?;
        db::kv_set(&mut tx, "instance_id", "library").await?;
        db::kv_set(&mut tx, "restore_accepted_at", &now.to_string()).await?;
        sqlx::query(
            "INSERT INTO actors(id,username,password_hash,role,status,revision,created_at)
            VALUES('owner','owner','x','owner','active',1,?)",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO backup_runs(id,state,snapshot_at,created_at,updated_at)
            VALUES('backup','succeeded',?,?,?)",
        )
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let source = serve(Router::new().route("/health/ready", get(|| async { "ok" }))
            .route("/api/v1/library/entry", get(move || async move {
                Json(json!({"instance_id":"library", "writes":"blocked", "frozen_at":now-10}))
            }))).await?;
        let target = serve(
            Router::new()
                .route("/health/ready", get(|| async { "ok" }))
                .route(
                    "/api/v1/library/entry",
                    get(move || async move {
                        Json(json!({"instance_id":"library", "writes":"open", "accepted_at":now}))
                    }),
                ),
        )
        .await?;
        let proxy = Arc::new(Mutex::new(json!({
            "handler":"reverse_proxy",
            "headers":{"request":{"set":{"X-Example":["preserved"]}}},
            "upstreams":[{"dial":dial(&source)?}],
        })));
        let path = "/config/apps/http/servers/gallery/routes/0/handle/0";
        let read = proxy.clone();
        let write = proxy.clone();
        let admin = serve(
            Router::new().route(
                path,
                get(move || async move {
                    (
                        [(header::ETAG, "\"gallery-etag\"")],
                        Json(read.lock().unwrap().clone()),
                    )
                })
                .patch(
                    move |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                        assert_eq!(headers[header::IF_MATCH], "\"gallery-etag\"");
                        *write.lock().unwrap() = body;
                        axum::http::StatusCode::OK
                    },
                ),
            ),
        )
        .await?;
        let ack = crate::ids::sha256_hex(format!("library:{}", now - 10).as_bytes());
        assert!(switch_caddy(
            &cfg,
            source.as_str(),
            target.as_str(),
            "wrong",
            admin.as_str(),
            path
        )
        .await
        .is_err());
        assert_eq!(
            proxy.lock().unwrap()["upstreams"][0]["dial"],
            dial(&source)?
        );
        let changed = switch_caddy(
            &cfg,
            source.as_str(),
            target.as_str(),
            &ack,
            admin.as_str(),
            path,
        )
        .await?;
        assert_eq!(changed["proxy_changed"], true);
        assert_eq!(
            proxy.lock().unwrap()["upstreams"][0]["dial"],
            dial(&target)?
        );
        assert_eq!(
            proxy.lock().unwrap()["headers"]["request"]["set"]["X-Example"][0],
            "preserved"
        );
        let replay = switch_caddy(
            &cfg,
            source.as_str(),
            target.as_str(),
            &ack,
            admin.as_str(),
            path,
        )
        .await?;
        assert_eq!(replay["proxy_changed"], false);
        Ok(())
    }
}
