use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;
use tower_http::limit::RequestBodyLimitLayer;

use crate::app::AppState;
use crate::auth::{self, Actor, Session};
use crate::error::{AppError, AppResult};
use crate::http_util::{self, cookie_header, parse_cookie};
use crate::ids::{random_token, sha256_hex};

const SESSION_COOKIE: &str = "gallery_session";
const SESSION_TTL_MS: i64 = 14 * 24 * 3600 * 1000;

#[derive(Clone)]
pub struct AuthUser(pub Option<Session>);

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/setup/status", get(setup_status))
        .route("/setup/owner", post(setup_owner))
        .route("/session", get(get_session).post(login).delete(logout))
        .route("/admin/account/password", patch(admin_password))
        .route("/account/password", patch(account_password))
        .route("/admin/users", get(list_users).post(create_user))
        .route("/admin/users/{user_id}", patch(patch_user))
        .route("/admin/users/{user_id}/reset-password", post(reset_password))
        .route("/admin/settings", get(admin_settings))
        .route("/admin/settings/{section}", patch(patch_settings))
        .route("/admin/runs/{id}", get(admin_run))
        .route("/admin/audit", get(admin_audit))
        .route("/admin/storage/probe", post(storage_probe))
        .route("/admin/reconcile", post(reconcile))
        .route("/admin/search/rebuild", post(rebuild_search))
        .route("/admin/jobs/summary", get(job_summary))
        .route("/admin/audit.jsonl", get(audit_export))
        .route("/admin/variants/stats", get(variant_stats))
        .route("/admin/job-pauses", post(pause_job_kind))
        .route("/admin/job-pauses/{kind}", delete(resume_job_kind))
        .route("/admin/backups", get(list_backups).post(create_backup))
        .route("/admin/backups/{id}", get(get_backup))
        .route("/admin/backups/{id}/verify", post(verify_backup))
        .route("/admin/backups/{id}/abort", post(abort_backup))
        .route("/admin/backups/{id}/resume", post(resume_backup))
        .route("/admin/restores", post(create_restore))
        .route("/admin/restores/{id}", get(get_restore))
        .route("/admin/restores/{id}/report", get(restore_report))
        .route("/admin/restores/{id}/abort", post(abort_restore))
        .route("/admin/restores/{id}/resume", post(resume_restore))
        .route("/admin/restores/{id}/promote", post(promote_restore))
        .route("/admin/recovery/early-snapshot", post(early_snapshot))
        .route("/admin/storage-migrations", post(plan_migration))
        .route("/admin/storage-migrations/{id}", get(get_migration))
        .route("/admin/storage-migrations/{id}/start", post(begin_migration))
        .route("/admin/storage-migrations/{id}/pause", post(pause_migration))
        .route("/admin/storage-migrations/{id}/resume", post(resume_migration))
        .route("/admin/storage-migrations/{id}/abort", post(abort_migration))
        .route("/admin/storage-migrations/{id}/rollback", post(rollback_migration))
        .route("/admin/storage-migrations/{id}/degrade", post(degrade_migration))
        .route("/admin/storage-migrations/{id}/cleanup", post(authorize_cleanup))
        .route("/admin/storage-migrations/{id}/items", get(migration_items))
        .route("/admin/storage-migrations/{id}/cutover", post(cutover_migration))
        .route("/admin/storage-migrations/{id}/retire", post(retire_migration))
        .route("/albums", get(list_albums).post(create_album))
        .route(
            "/albums/{album_id}",
            get(get_album).patch(patch_album).delete(delete_album),
        )
        .route("/albums/{album_id}/access", patch(patch_access))
        .route("/albums/{album_id}/images", get(list_images))
        .route("/albums/{album_id}/images/{image_id}", get(get_image))
        .route(
            "/albums/{album_id}/images/{image_id}/variants/{variant_id}/body",
            get(variant_body),
        )
        .route(
            "/albums/{album_id}/images/{image_id}/original-view",
            get(original_view),
        )
        .route(
            "/albums/{album_id}/images/{image_id}/original-download",
            get(original_download),
        )
        .route("/albums/{album_id}/items/add", post(items_add))
        .route("/albums/{album_id}/items/remove", post(items_remove))
        .route("/albums/{album_id}/items/move", post(items_move))
        .route("/albums/{album_id}/items/reorder", post(items_reorder))
        .route("/albums/{album_id}/items/rename", post(items_rename))
        .route("/albums/{album_id}/uploads", post(create_upload))
        .route("/albums/{album_id}/downloads/selected", post(selected_download))
        .route("/albums/{album_id}/archives", post(create_archive))
        .route("/albums/{album_id}/restore", post(restore_album))
        .route("/uploads", get(list_uploads))
        .route("/uploads/{batch_id}", get(get_upload).delete(cancel_upload))
        .route(
            "/uploads/{batch_id}/items/{item_id}/body",
            put(upload_body),
        )
        .route("/downloads/claims/{claim_id}/body", get(claim_body))
        .route("/archives", get(list_archives))
        .route("/archives/{id}", get(get_archive))
        .route("/archives/{id}/body", get(archive_body))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/jobs/{id}/retry", post(retry_job))
        .route("/capabilities", get(capabilities))
        .route("/public/profile", get(portfolio_profile))
        .route("/public/collections", get(portfolio_collections))
        .route("/public/works", get(portfolio_works))
        .route("/admin/settings/presentation", patch(update_portfolio_profile))
        .route(
            "/variant-profiles/current",
            get(get_profile).put(put_profile),
        )
        .route("/conversions", post(create_conversion))
        .route("/conversions/{id}", get(get_conversion))
        .route(
            "/conversions/{id}/outputs/{output_id}/body",
            get(conversion_body),
        )
        .route("/images/{image_id}/caption", patch(patch_caption))
        .route("/images/{image_id}/trash", post(trash_image))
        .route("/images/{image_id}/restore", post(restore_image))
        .route("/images/{image_id}/purge", post(purge_image))
        .route("/library/entry", get(library_entry))
        .route("/admin/source/freeze", post(freeze_source))
        .route("/library/images", get(library_images))
        .route("/library/images/{image_id}", get(library_image))
        .route(
            "/library/images/{image_id}/variants/{variant_id}/body",
            get(library_variant_body),
        )
        .route(
            "/library/images/{image_id}/original-view",
            get(library_original_view),
        )
        .route(
            "/library/images/{image_id}/original-download",
            get(library_original_download),
        )
        .route("/trash", get(list_trash))
        .route("/search", get(search))
        .route("/ai/scans", post(ai_scan))
        .route("/ai/scans/{id}", get(get_scan))
        .route("/ai/scans/{id}/cancel", post(cancel_scan))
        .route("/agent/sessions", get(list_agent).post(create_agent))
        .route("/agent/sessions/{id}", get(get_agent))
        .route("/agent/sessions/{id}/dispatch", post(agent_dispatch))
        .route("/agent/sessions/{id}/messages", get(agent_messages).post(agent_post_message))
        .route(
            "/agent/sessions/{id}/messages/{message_id}/remove",
            post(agent_remove_message),
        )
        .route(
            "/agent/sessions/{id}/messages/{message_id}/resend",
            post(agent_resend_message),
        )
        .route(
            "/agent/sessions/{id}/turns/{turn_id}/stop",
            post(agent_stop_turn),
        )
        .route("/agent/sessions/{id}/resume-pending", post(agent_resume_pending))
        .route("/agent/sessions/{id}/archive", post(agent_archive))
        .route(
            "/agent/sessions/{id}/actions/{action_id}/approve",
            post(agent_approve),
        )
        .route(
            "/agent/sessions/{id}/actions/{action_id}/reject",
            post(agent_reject),
        )
        .route("/events", get(sse_events))
        .layer(RequestBodyLimitLayer::new(110 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            password_change_gate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            readonly_gate,
        ))
        .layer(axum::middleware::from_fn(rewrite_colon_actions))
        .layer(axum::middleware::map_response(json_rejection_as_problem))
        .layer(axum::middleware::from_fn(limit_json_body));

    let faults = state.clone();
    let app = Router::new();
    let app = if let Some(root) = &state.config.web_root {
        let assets = axum::Router::new().fallback_service(
            tower_http::services::ServeDir::new(root.join("assets"))
                .append_index_html_on_directories(false),
        ).layer(axum::middleware::map_response(|mut response: Response| async move {
            if response.status().is_success() {
                response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
            }
            response
        }));
        app.nest("/assets", assets)
    } else {
        app
    };
    app.route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .nest("/api/v1", api)
        .fallback(admin_or_spa)
        .layer(axum::middleware::from_fn(rewrite_colon_actions))
        .layer(axum::middleware::from_fn_with_state(faults, record_request_faults))
        .with_state(state)
}

async fn record_request_faults(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    let status = response.status();
    if status.is_server_error() && !path.starts_with("/health/") {
        let _ = sqlx::query("INSERT INTO request_faults(status, occurred_at) VALUES(?, ?)")
            .bind(i64::from(status.as_u16()))
            .bind(crate::db::now_ms())
            .execute(&state.pool)
            .await;
    }
    response
}

pub fn rewrite_colon_path(path: &str) -> String {
    path.replace(':', "/")
}

fn password_change_allowed(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path.ends_with("/session")
        || path.ends_with("/account/password")
        || path.ends_with("/admin/account/password")
        || path.ends_with("/setup/status")
        || path.ends_with("/setup/owner")
        || path.ends_with("/capabilities")
        || path.ends_with("/public/profile")
        || path.ends_with("/public/collections")
        || path.ends_with("/public/works")
}

fn readonly_mutation_allowed(method: &Method, path: &str) -> bool {
    if matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS) {
        return true;
    }
    let path = path.trim_end_matches('/');
    path.contains("/admin/backups")
        || path.contains("/admin/restores")
        || path.contains("/admin/recovery")
        || path.contains("/admin/storage")
        || path.contains("/admin/source")
        || path.ends_with("/session")
        || path.ends_with("/account/password")
        || path.ends_with("/admin/account/password")
        || path.contains("/setup/")
}

async fn readonly_gate(
    State(state): State<AppState>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    if readonly_mutation_allowed(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    match crate::db::source_readonly(&state.pool).await {
        Ok(false) => next.run(req).await,
        Ok(true) => AppError::conflict(
            "SOURCE_READONLY",
            "the source stays read-only until handoff finishes",
        )
        .into_response(),
        Err(_) => AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
            "Service Unavailable",
            "the read-only state could not be read",
        )
        .into_response(),
    }
}

async fn library_entry(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(crate::backup::readonly_entry(&state.pool).await?))
}

async fn freeze_source(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(crate::backup::freeze_source(&state.pool, &s.actor).await?))
}

async fn password_change_gate(
    State(state): State<AppState>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    if !password_change_allowed(req.uri().path()) {
        if let Some(session) = session_from(&state, req.headers()).await {
            if session.actor.must_change_password {
                return AppError::new(
                    StatusCode::FORBIDDEN,
                    "MUST_CHANGE_PASSWORD",
                    "Forbidden",
                    "change the password before using the gallery",
                )
                .into_response();
            }
        }
    }
    next.run(req).await
}

async fn rewrite_colon_actions(mut req: Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    if path.contains(':') {
        let rewritten = rewrite_colon_path(&path);
        let query = req
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let mut parts = req.uri().clone().into_parts();
        if let Ok(pq) = format!("{rewritten}{query}").parse() {
            parts.path_and_query = Some(pq);
            if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    next.run(req).await
}

const JSON_BODY_LIMIT: u64 = 1024 * 1024;

async fn limit_json_body(request: Request, next: axum::middleware::Next) -> Response {
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .starts_with("application/json");
    if is_json {
        if let Some(length) = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            if length > JSON_BODY_LIMIT {
                return AppError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "JSON_TOO_LARGE",
                    "Payload Too Large",
                    "JSON body exceeds 1 MiB",
                )
                .into_response();
            }
        }
    }
    next.run(request).await
}

async fn json_rejection_as_problem(response: Response) -> Response {
    let status = response.status();
    if status != StatusCode::UNPROCESSABLE_ENTITY && status != StatusCode::BAD_REQUEST {
        return response;
    }
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .contains("problem+json")
    {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 4096).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    if text.contains("deserialize the JSON body") {
        return AppError::unprocessable("JSON_BODY", "unknown or invalid field").into_response();
    }
    if text.contains("parse the request body as JSON") {
        return AppError::bad_request("JSON", "invalid json").into_response();
    }
    Response::from_parts(parts, Body::from(bytes.to_vec()))
}

async fn live() -> impl IntoResponse {
    StatusCode::OK
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let (ok, body) = crate::health::readiness(&state).await;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

async fn session_from(state: &AppState, headers: &HeaderMap) -> Option<Session> {
    let raw = parse_cookie(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
        SESSION_COOKIE,
    )?;
    auth::lookup_session(&state.pool, raw).await.ok().flatten()
}

async fn require_user(state: &AppState, headers: &HeaderMap) -> AppResult<Session> {
    session_from(state, headers)
        .await
        .ok_or_else(AppError::unauthorized)
}

fn if_match_revision(headers: &HeaderMap) -> AppResult<Option<i64>> {
    let Some(raw) = headers.get(header::IF_MATCH).and_then(|value| value.to_str().ok()) else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.starts_with("W/") || raw.contains(',') {
        return Err(AppError::bad_request("WEAK_ETAG", "If-Match must be one strong ETag"));
    }
    let inner = raw.trim_matches('"');
    if inner == "*" || inner.is_empty() {
        return Err(AppError::bad_request("ETAG", "If-Match is not a revision"));
    }
    inner
        .parse::<i64>()
        .map(Some)
        .map_err(|_| AppError::bad_request("ETAG", "If-Match is not a revision"))
}

fn resolve_revision(headers: &HeaderMap, json_expected: Option<i64>) -> AppResult<i64> {
    match (if_match_revision(headers)?, json_expected) {
        (Some(header), Some(json)) if header != json => Err(AppError::precondition(
            "REVISION_MISMATCH",
            "If-Match and expected_revision differ",
        )),
        (Some(header), _) => Ok(header),
        (None, Some(json)) => Ok(json),
        (None, None) => Err(AppError::new(
            StatusCode::PRECONDITION_REQUIRED,
            "REVISION_REQUIRED",
            "Precondition Required",
            "expected_revision required",
        )),
    }
}

fn with_revision_etag(status: StatusCode, body: serde_json::Value) -> Response {
    let mut response = (status, Json(body.clone())).into_response();
    if let Some(revision) = body.get("revision").and_then(|value| value.as_i64()) {
        if let Ok(etag) = format!("\"{revision}\"").parse() {
            response.headers_mut().insert(header::ETAG, etag);
        }
    }
    response
}

fn check_csrf(state: &AppState, headers: &HeaderMap, session: &Session) -> AppResult<()> {
    let token = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if token != session.csrf_token {
        return Err(AppError::forbidden());
    }
    if let Some(origin) = http_util::header_origin(headers) {
        if !http_util::origin_allowed(&origin, &state.config.public_url) {
            return Err(AppError::forbidden());
        }
    }
    Ok(())
}

async fn portfolio_profile(State(state): State<AppState>) -> AppResult<Json<crate::portfolio::Profile>> {
    Ok(Json(crate::portfolio::profile(&state.pool).await?))
}

async fn portfolio_collections(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({"items": crate::portfolio::collections(&state.pool).await?})))
}

#[derive(Deserialize)]
struct PortfolioQuery {
    album_id: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

async fn portfolio_works(
    State(state): State<AppState>,
    Query(query): Query<PortfolioQuery>,
) -> AppResult<Json<crate::portfolio::WorkPage>> {
    Ok(Json(crate::portfolio::works(&state.pool, query.album_id.as_deref(), query.limit.unwrap_or(48), query.cursor.as_deref()).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortfolioInput {
    title: String,
    description: String,
    expected_revision: i64,
}

async fn update_portfolio_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PortfolioInput>,
) -> AppResult<Json<crate::portfolio::Profile>> {
    let user = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &user)?;
    auth::require_owner(&user.actor)?;
    Ok(Json(crate::portfolio::write_profile(&state.pool, &body.title, &body.description, body.expected_revision).await?))
}

async fn setup_status(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({
        "initialized": auth::initialized(&state.pool).await.map_err(anyhow::Error::from)?
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupOwner {
    bootstrap_token: String,
    username: Option<String>,
    password: String,
}

async fn setup_owner(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetupOwner>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let source = client_source(&headers);
    if let Some(secs) = auth::auth_retry_after(
        &state.pool,
        "setup",
        "bootstrap",
        &source,
        auth::SETUP_FAILURE_LIMIT,
    )
    .await
    .map_err(anyhow::Error::from)?
    {
        return Err(AppError::rate_limited(secs));
    }
    if auth::initialized(&state.pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict("ALREADY_INITIALIZED", "owner exists"));
    }
    if !auth::password_ok(&body.password) {
        return Err(AppError::unprocessable("WEAK_PASSWORD", "password too short"));
    }
    let sha = sha256_hex(body.bootstrap_token.as_bytes());
    let now = crate::db::now_ms();
    let tok = sqlx::query_as::<_, (i64, Option<i64>)>(
        "SELECT expires_at, consumed_at FROM bootstrap_tokens WHERE token_sha256 = ?",
    )
    .bind(&sha)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let rejected = match tok {
        None => true,
        Some((expires, consumed)) => consumed.is_some() || expires < now,
    };
    if rejected {
        if let Some(secs) = auth::record_auth_failure(
            &state.pool,
            "setup",
            "bootstrap",
            &source,
            auth::SETUP_FAILURE_LIMIT,
        )
        .await
        .map_err(anyhow::Error::from)?
        {
            return Err(AppError::rate_limited(secs));
        }
        return Err(AppError::unauthorized());
    }
    let username = body
        .username
        .unwrap_or_else(|| state.config.admin_username.clone());
    auth::create_owner(&state.pool, &username, &body.password)
        .await
        .map_err(anyhow::Error::from)?;
    auth::clear_auth_failures(&state.pool, "setup", "bootstrap", &source)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE bootstrap_tokens SET consumed_at = ? WHERE token_sha256 = ?")
        .bind(now)
        .bind(&sha)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"initialized": true})),
    ))
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let s = session_from(&state, &headers).await;
    Ok(Json(session_json(s.as_ref())))
}

fn client_source(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 80)
        .unwrap_or("local")
        .to_string()
}

fn session_json(s: Option<&Session>) -> serde_json::Value {
    match s {
        None => serde_json::json!({"actor": null}),
        Some(s) => serde_json::json!({
            "actor": {
                "id": s.actor.id,
                "username": s.actor.username,
                "role": s.actor.role,
                "must_change_password": s.actor.must_change_password
            },
            "capability_revision": s.actor.revision,
            "expires_at": crate::db::rfc3339(s.expires_at),
            "csrf_token": s.csrf_token,
            "event_cursor": s.event_cursor
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> AppResult<Response> {
    if !auth::initialized(&state.pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict("NOT_INITIALIZED", "setup required"));
    }
    let source = client_source(&headers);
    let subject = body.username.trim();
    if let Some(secs) = auth::auth_retry_after(
        &state.pool,
        "login",
        subject,
        &source,
        auth::LOGIN_FAILURE_LIMIT,
    )
    .await
    .map_err(anyhow::Error::from)?
    {
        return Err(AppError::rate_limited(secs));
    }
    let found = auth::find_actor_by_username(&state.pool, subject)
        .await
        .map_err(anyhow::Error::from)?;
    let accepted = match found {
        Some((actor, hash)) if auth::verify_password(&hash, &body.password) => Some(actor),
        _ => None,
    };
    let Some(actor) = accepted else {
        if let Some(secs) = auth::record_auth_failure(
            &state.pool,
            "login",
            subject,
            &source,
            auth::LOGIN_FAILURE_LIMIT,
        )
        .await
        .map_err(anyhow::Error::from)?
        {
            return Err(AppError::rate_limited(secs));
        }
        return Err(AppError::unauthorized());
    };
    auth::clear_auth_failures(&state.pool, "login", subject, &source)
        .await
        .map_err(anyhow::Error::from)?;
    let (raw, session) = auth::issue_session(&state.pool, &actor, SESSION_TTL_MS)
        .await
        .map_err(anyhow::Error::from)?;
    let mut res = (
        StatusCode::CREATED,
        Json(session_json(Some(&session))),
    )
        .into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        cookie_header(
            SESSION_COOKIE,
            &raw,
            SESSION_TTL_MS / 1000,
            state.config.cookie_secure(),
        )
        .parse()
        .unwrap(),
    );
    Ok(res)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> AppResult<StatusCode> {
    if let Some(s) = session_from(&state, &headers).await {
        auth::revoke_session(&state.pool, &s.id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordBody {
    current_password: String,
    new_password: String,
    confirm_password: Option<String>,
}

async fn admin_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> AppResult<StatusCode> {
    let session = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &session)?;
    auth::require_owner(&session.actor)?;
    change_password(&state, &session.actor, &body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn account_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> AppResult<StatusCode> {
    let session = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &session)?;
    change_password(&state, &session.actor, &body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn change_password(state: &AppState, actor: &Actor, body: &PasswordBody) -> AppResult<()> {
    if body.confirm_password.as_deref().unwrap_or(&body.new_password) != body.new_password {
        return Err(AppError::unprocessable("PASSWORD_MISMATCH", "confirm mismatch"));
    }
    if !auth::password_ok(&body.new_password) {
        return Err(AppError::unprocessable("WEAK_PASSWORD", "password too short"));
    }
    let row: (String,) = sqlx::query_as("SELECT password_hash FROM actors WHERE id = ?")
        .bind(&actor.id)
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if !auth::verify_password(&row.0, &body.current_password) {
        return Err(AppError::unauthorized());
    }
    let hash = auth::hash_password(&body.new_password).map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE actors SET password_hash = ?, must_change_password = 0, revision = revision + 1 WHERE id = ?")
        .bind(&hash)
        .bind(&actor.id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    auth::revoke_actor_sessions(&state.pool, &actor.id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUser {
    username: String,
    password: String,
}

async fn list_users(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    auth::require_owner(&s.actor)?;
    let rows = sqlx::query_as::<_, (String, String, String, i64, i64)>(
        "SELECT id, username, role, status = 'active', revision FROM actors ORDER BY created_at",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::json!({
        "items": rows.into_iter().map(|(id, username, role, active, revision)| serde_json::json!({
            "id": id, "username": username, "role": role, "active": active == 1, "revision": revision
        })).collect::<Vec<_>>(),
        "next_cursor": null
    })))
}

async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUser>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    if !auth::password_ok(&body.password) {
        return Err(AppError::unprocessable("WEAK_PASSWORD", "password too short"));
    }
    let hash = auth::hash_password(&body.password).map_err(anyhow::Error::from)?;
    let id = crate::ids::new_id();
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, status, must_change_password, revision, created_at)
         VALUES(?, ?, ?, 'collaborator', 'active', 1, 1, ?)",
    )
    .bind(&id)
    .bind(&body.username)
    .bind(&hash)
    .bind(crate::db::now_ms())
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"id": id, "username": body.username, "revision": 1})),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchUser {
    expected_revision: Option<i64>,
    status: Option<String>,
}

async fn patch_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<PatchUser>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    let expected = resolve_revision(&headers, body.expected_revision)?;
    let status = body.status.as_deref().ok_or_else(|| {
        AppError::unprocessable("ACCOUNT_STATUS", "status must be active or disabled")
    })?;
    if status != "active" && status != "disabled" {
        return Err(AppError::unprocessable(
            "ACCOUNT_STATUS",
            "status must be active or disabled",
        ));
    }
    let current = sqlx::query_as::<_, (String, i64)>(
        "SELECT role, revision FROM actors WHERE id = ?",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if current.0 == "owner" {
        return Err(AppError::conflict("ACCOUNT_OWNER", "the owner account stays on its own password path"));
    }
    if current.1 != expected {
        return Err(AppError::precondition("REVISION_CONFLICT", "account revision"));
    }
    let updated = sqlx::query(
        "UPDATE actors SET status = ?, revision = revision + 1 WHERE id = ? AND revision = ?",
    )
    .bind(status)
    .bind(&user_id)
    .bind(expected)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if updated.rows_affected() != 1 {
        return Err(AppError::precondition("REVISION_CONFLICT", "account revision"));
    }
    if status == "disabled" {
        auth::revoke_actor_sessions(&state.pool, &user_id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    Ok(Json(serde_json::json!({
        "id": user_id,
        "status": status,
        "revision": expected + 1
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetPassword {
    password: String,
    expected_revision: Option<i64>,
}

async fn reset_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<ResetPassword>,
) -> AppResult<StatusCode> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    let expected = resolve_revision(&headers, body.expected_revision)?;
    if !auth::password_ok(&body.password) {
        return Err(AppError::unprocessable("WEAK_PASSWORD", "password too short"));
    }
    let current = sqlx::query_as::<_, (String, i64)>(
        "SELECT role, revision FROM actors WHERE id = ?",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if current.0 == "owner" {
        return Err(AppError::conflict("ACCOUNT_OWNER", "the owner account stays on its own password path"));
    }
    if current.1 != expected {
        return Err(AppError::precondition("REVISION_CONFLICT", "account revision"));
    }
    let hash = auth::hash_password(&body.password).map_err(anyhow::Error::from)?;
    let updated = sqlx::query(
        "UPDATE actors SET password_hash = ?, must_change_password = 1, revision = revision + 1
         WHERE id = ? AND revision = ?",
    )
    .bind(&hash)
    .bind(&user_id)
    .bind(expected)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if updated.rows_affected() != 1 {
        return Err(AppError::precondition("REVISION_CONFLICT", "account revision"));
    }
    auth::revoke_actor_sessions(&state.pool, &user_id)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_settings(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    auth::require_owner(&s.actor)?;
    let limits = crate::settings::limits(&state.pool).await?;
    let kind = state.media().await.map(|media| media.kind).unwrap_or_else(|_| "unknown".into());
    Ok(Json(serde_json::json!({
        "revision": limits.revision,
        "storage": {"kind": kind, "source": "sqlite", "writable": false},
        "ai": {"enabled": state.config.ai_enabled, "source": "env", "writable": false},
        "limits": crate::settings::limits_json(&limits)
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsPatch {
    expected_revision: Option<i64>,
    max_upload_items: Option<i64>,
    upload_concurrency_hint: Option<i64>,
    max_download_selection: Option<i64>,
    ai_daily_token_budget: Option<i64>,
}

async fn patch_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(section): Path<String>,
    Json(body): Json<SettingsPatch>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    if section == "storage" || section == "ai" {
        return Err(AppError::conflict(
            "SETTINGS_READONLY",
            "this section comes from the environment or the storage route",
        ));
    }
    if section != "limits" {
        return Err(AppError::not_found());
    }
    let expected = resolve_revision(&headers, body.expected_revision)?;
    let limits = crate::settings::patch_limits(
        &state.pool,
        expected,
        body.max_upload_items,
        body.upload_concurrency_hint,
        body.max_download_selection,
        body.ai_daily_token_budget,
    )
    .await?;
    Ok(Json(serde_json::json!({
        "revision": limits.revision,
        "limits": crate::settings::limits_json(&limits)
    })))
}

async fn admin_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    auth::require_owner(&s.actor)?;
    if let Some(job) = crate::jobs::get(&state.pool, &id)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Ok(Json(serde_json::json!({
            "kind": job.kind,
            "resource_id": job.id,
            "state": job.state,
            "progress": job.progress,
            "detail_url": job.status_url
        })));
    }
    Err(AppError::not_found())
}

async fn admin_audit(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    auth::require_owner(&s.actor)?;
    let rows = sqlx::query_as::<_, (i64, String, Option<String>, String)>(
        "SELECT id, action, target, result FROM audit_log ORDER BY id DESC LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::json!({
        "items": rows.into_iter().map(|(id, action, target, result)| serde_json::json!({
            "id": id, "action": action, "target": target, "result": result
        })).collect::<Vec<_>>()
    })))
}

async fn rebuild_search(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    let existing = sqlx::query_as::<_, (String, String)>(
        "SELECT id, state FROM jobs
         WHERE kind = 'rebuild_search' AND state IN ('queued', 'running', 'retry_wait')
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some((id, job_state)) = existing {
        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "job_id": id,
                "state": job_state,
                "status_url": format!("/api/v1/jobs/{id}")
            })),
        ));
    }
    let payload = crate::jobs::JobPayload {
        kind: "rebuild_search".into(),
        payload_version: 1,
        scope: serde_json::json!({}),
        target: serde_json::json!({}),
        actor_id: Some(s.actor.id.clone()),
    };
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    let job_id = crate::jobs::enqueue(
        &mut tx,
        "rebuild_search",
        &format!("rebuild_search:{}", crate::ids::new_id()),
        &payload,
        Some(&s.actor.id),
        20,
    )
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "job_id": job_id,
            "state": "queued",
            "status_url": format!("/api/v1/jobs/{job_id}")
        })),
    ))
}

async fn storage_probe(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    auth::require_owner(&s.actor)?;
    let caps = state
        .storage
        .backend()
        .probe()
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::to_value(caps).unwrap()))
}

async fn reconcile(State(state): State<AppState>, headers: HeaderMap) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    let existing = sqlx::query_as::<_, (String, String)>(
        "SELECT id, state FROM jobs
         WHERE kind = 'reconcile' AND state IN ('queued', 'running', 'retry_wait')
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some((id, job_state)) = existing {
        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "job_id": id,
                "state": job_state,
                "status_url": format!("/api/v1/jobs/{id}")
            })),
        ));
    }
    let media = state.media().await.map_err(anyhow::Error::from)?;
    let job_key = format!("reconcile:{}:{}", media.backend_id, crate::ids::new_id());
    let payload = crate::jobs::JobPayload {
        kind: "reconcile".into(),
        payload_version: 1,
        scope: serde_json::json!({"backend_id": media.backend_id}),
        target: serde_json::json!({}),
        actor_id: Some(s.actor.id.clone()),
    };
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    let job_id = crate::jobs::enqueue(&mut tx, "reconcile", &job_key, &payload, Some(&s.actor.id), 40)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "job_id": job_id,
            "state": "queued",
            "status_url": format!("/api/v1/jobs/{job_id}")
        })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PauseJobBody {
    kind: String,
    reason: String,
}

async fn job_summary(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    auth::require_owner(&s.actor)?;
    Ok(Json(crate::jobs::summary(&state.pool).await.map_err(anyhow::Error::from)?))
}

async fn variant_stats(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    auth::require_owner(&s.actor)?;
    Ok(Json(
        crate::versions::publication_stats(&state.pool)
            .await
            .map_err(anyhow::Error::from)?,
    ))
}

async fn pause_job_kind(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PauseJobBody>,
) -> AppResult<StatusCode> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    crate::jobs::pause_kind(&state.pool, &s.actor.id, &body.kind, &body.reason)
        .await
        .map_err(|_| AppError::unprocessable("JOB_PAUSE", "kind or reason is not acceptable"))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_job_kind(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
) -> AppResult<StatusCode> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    if !crate::jobs::resume_kind(&state.pool, &kind).await.map_err(anyhow::Error::from)? {
        return Err(AppError::not_found());
    }
    state.wake_jobs();
    Ok(StatusCode::NO_CONTENT)
}

async fn list_backups(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    let free = crate::download::free_bytes(&state.config.backup_root());
    Ok(Json(crate::backup::list_backups(&state.pool, &s.actor, free).await?))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BackupQuery {
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    backup_repository_id: Option<String>,
}

async fn create_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<BackupQuery>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if query.dry_run {
        let value = crate::backup::preview_backup(&state, &s.actor, query.backup_repository_id.as_deref()).await?;
        return Ok((StatusCode::OK, Json(value)).into_response());
    }
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        "/admin/backups",
        &headers,
        serde_json::json!({}),
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::backup::start_backup(&state, &s.actor).await?;
    cmd.remember(&state.pool, 202, &value).await?;
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

async fn abort_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    crate::auth::require_owner(&s.actor)?;
    crate::backup::abandon_unsealed(&state.pool, &id).await?;
    Ok(Json(serde_json::json!({"id": id, "state": "abandoned"})))
}

async fn resume_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let value = crate::backup::resume_backup(&state, &s.actor, &id).await?;
    state.wake_jobs();
    Ok(Json(value))
}

async fn verify_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(crate::backup::verify_backup(&state, &s.actor, &id).await?))
}

async fn get_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::backup::get_backup(&state.pool, &s.actor, &id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreBody {
    backup_id: String,
    #[serde(default)]
    confirm_loss: Option<String>,
    #[serde(default)]
    branch_id: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

async fn early_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let salvage = state.config.work_root().join("salvage");
    Ok(Json(
        crate::backup::assess_early_snapshot(
            &state.pool,
            &s.actor,
            &state.config.backup_root(),
            &salvage,
        )
        .await?,
    ))
}

async fn create_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RestoreBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    let mode = body.mode.as_deref().unwrap_or("execute");
    if mode == "preview" {
        let plan = crate::backup::restore_plan(
            &state.pool,
            &s.actor,
            &body.backup_id,
            body.branch_id.as_deref(),
            body.confirm_loss.as_deref(),
            false,
        )
        .await?;
        return Ok((StatusCode::OK, Json(plan)));
    }
    if mode != "dry_run" && mode != "execute" {
        return Err(AppError::conflict("RESTORE_MODE", "mode is preview, dry_run, or execute"));
    }
    if crate::db::write_barrier(&state.pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict(
            "MAINTENANCE",
            "another maintenance operation holds the lock",
        ));
    }
    let plan = crate::backup::restore_plan(
        &state.pool,
        &s.actor,
        &body.backup_id,
        body.branch_id.as_deref(),
        body.confirm_loss.as_deref(),
        true,
    )
    .await?;
    if mode == "dry_run" {
        return Ok((StatusCode::OK, Json(plan)));
    }
    let branch = &plan;
    let id = crate::ids::new_id();
    sqlx::query(
        "INSERT INTO restore_runs(id, backup_id, parent_backup_id, branch_id, state, created_at, updated_at)
         VALUES(?, ?, ?, ?, 'queued', ?, ?)",
    )
    .bind(&id)
    .bind(&body.backup_id)
    .bind(branch["parent_backup_id"].as_str())
    .bind(branch["branch_id"].as_str())
    .bind(crate::db::now_ms())
    .bind(crate::db::now_ms())
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    let payload = crate::jobs::JobPayload {
        kind: "restore".into(),
        payload_version: 1,
        scope: serde_json::json!({
            "restore_id": id,
            "backup_id": body.backup_id,
            "parent_backup_id": branch["parent_backup_id"],
            "branch_id": branch["branch_id"]
        }),
        target: serde_json::json!({}),
        actor_id: Some(s.actor.id.clone()),
    };
    let job_id = crate::jobs::enqueue(&mut tx, "restore", &format!("restore:{id}"), &payload, Some(&s.actor.id), 5)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE restore_runs SET job_id = ? WHERE id = ?")
        .bind(&job_id)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "id": id,
            "job_id": job_id,
            "parent_backup_id": branch["parent_backup_id"],
            "branch_id": branch["branch_id"]
        })),
    ))
}

async fn restore_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(
        crate::backup::export_restore_report(&state, &s.actor, &id).await?,
    ))
}

async fn get_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::backup::get_restore(&state.pool, &s.actor, &id).await?))
}

async fn abort_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(crate::backup::abort_restore(&state, &s.actor, &id).await?))
}

async fn resume_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let value = crate::backup::resume_restore(&state, &s.actor, &id).await?;
    state.wake_jobs();
    Ok(Json(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromoteBody {
    expected_revision: i64,
    source_ack: String,
}

async fn promote_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PromoteBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(
        crate::backup::promote_restore(
            &state,
            &s.actor,
            &id,
            body.expected_revision,
            &body.source_ack,
        )
        .await?,
    ))
}

async fn plan_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if let Some(obj) = body.as_object_mut() {
        obj.remove("dry_run");
    }
    let target: crate::storage::BackendConfig = serde_json::from_value(body.clone()).map_err(|_| {
        AppError::bad_request("STORAGE_TARGET", "target backend is incomplete")
    })?;
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        "/admin/storage-migrations",
        &headers,
        body,
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::migrate_storage::plan(&state, &s.actor, target).await?;
    cmd.remember(&state.pool, 200, &value).await?;
    Ok((StatusCode::OK, Json(value)).into_response())
}

async fn begin_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CutoverBody>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let payload = serde_json::json!({"expected_revision": body.expected_revision});
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        format!("/admin/storage-migrations/{id}/start"),
        &headers,
        payload,
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::migrate_storage::begin(&state, &s.actor, &id, body.expected_revision).await?;
    cmd.remember(&state.pool, 202, &value).await?;
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

async fn get_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::migrate_storage::get(&state.pool, &s.actor, &id).await?))
}

async fn pause_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(crate::migrate_storage::pause(&state, &s.actor, &id).await?))
}

async fn resume_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(crate::migrate_storage::resume(&state, &s.actor, &id).await?))
}

async fn abort_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(crate::migrate_storage::abort(&state, &s.actor, &id).await?))
}

async fn rollback_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let value = crate::migrate_storage::rollback(&state, &s.actor, &id).await?;
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DegradeBody {
    blob_ids: Vec<String>,
    backup_id: String,
}

async fn degrade_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<DegradeBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(
        crate::migrate_storage::degrade(&state, &s.actor, &id, &body.blob_ids, &body.backup_id).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupBody {
    kind: String,
}

async fn authorize_cleanup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CleanupBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(
        crate::migrate_storage::authorize_cleanup(&state.pool, &s.actor, &id, &body.kind).await?,
    ))
}

async fn migration_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(
        crate::migrate_storage::items(
            &state.pool,
            &s.actor,
            &id,
            q.limit.unwrap_or(50),
            q.cursor.as_deref(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CutoverBody {
    expected_revision: i64,
}

async fn cutover_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CutoverBody>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let value = crate::migrate_storage::cutover(&state, &s.actor, &id, body.expected_revision).await?;
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetireBody {
    #[serde(default)]
    dry_run: bool,
    expected_revision: Option<i64>,
    preview_sha: Option<String>,
}

async fn retire_migration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RetireBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(
        crate::migrate_storage::retire(
            &state,
            &s.actor,
            &id,
            body.dry_run,
            body.expected_revision,
            body.preview_sha.as_deref(),
        )
        .await?,
    ))
}

async fn list_albums(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let actor = session_from(&state, &headers).await.map(|s| s.actor);
    let items = crate::album::list(&state.pool, actor.as_ref()).await?;
    Ok(Json(serde_json::json!({"items": items, "next_cursor": null})))
}

async fn create_album(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<crate::album::CreateAlbum>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let payload = serde_json::to_value(&body).map_err(anyhow::Error::from)?;
    let cmd = crate::idempotency::Command::prepare(&s.actor.id, "POST", "/albums", &headers, payload);
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let view = crate::album::create(&state.pool, &s.actor, body).await?;
    let value = serde_json::to_value(&view).map_err(anyhow::Error::from)?;
    cmd.remember(&state.pool, 201, &value).await?;
    Ok((StatusCode::CREATED, Json(value)).into_response())
}

fn replay_response(hit: crate::idempotency::Replay) -> Response {
    with_revision_etag(
        StatusCode::from_u16(hit.status).unwrap_or(StatusCode::OK),
        hit.body,
    )
}

async fn get_album(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
) -> AppResult<Response> {
    let actor = session_from(&state, &headers).await.map(|s| s.actor);
    let view = crate::album::get(&state.pool, actor.as_ref(), &album_id).await?;
    let mut value = serde_json::to_value(&view).map_err(anyhow::Error::from)?;
    if actor.as_ref().is_some_and(|actor| actor.role == "owner") {
        let grants: Vec<(String, bool, bool, bool, bool, bool)> = sqlx::query_as(
            "SELECT actor_id, browse, edit, upload, view_original, download FROM album_grants
             WHERE album_id = ? ORDER BY actor_id",
        ).bind(&album_id).fetch_all(&state.pool).await.map_err(anyhow::Error::from)?;
        value["grants"] = serde_json::json!(grants.into_iter().map(|(actor_id, browse, edit, upload, view_original, download)| {
            serde_json::json!({ "actor_id": actor_id, "browse": browse, "edit": edit,
                "upload": upload, "view_original": view_original, "download": download })
        }).collect::<Vec<_>>());
    }
    Ok(with_revision_etag(StatusCode::OK, value))
}

async fn patch_album(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<crate::album::PatchAlbum>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let revision = resolve_revision(&headers, body.expected_revision)?;
    let mut body = body;
    body.expected_revision = Some(revision);
    let payload = serde_json::to_value(&body).map_err(anyhow::Error::from)?;
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "PATCH",
        format!("/albums/{album_id}"),
        &headers,
        payload,
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let view = crate::album::patch(&state.pool, &s.actor, &album_id, body, None).await?;
    let value = serde_json::to_value(&view).map_err(anyhow::Error::from)?;
    cmd.remember(&state.pool, 200, &value).await?;
    Ok(with_revision_etag(StatusCode::OK, value))
}

async fn delete_album(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
) -> AppResult<StatusCode> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let revision = resolve_revision(&headers, None)?;
    crate::album::soft_delete(&state.pool, &s.actor, &album_id, revision).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn patch_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<crate::album::AccessPatch>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let revision = resolve_revision(&headers, body.expected_revision)?;
    let mut body = body;
    body.expected_revision = Some(revision);
    let view = crate::album::patch_access(&state.pool, &s.actor, &album_id, body).await?;
    let value = serde_json::to_value(&view).map_err(anyhow::Error::from)?;
    Ok(with_revision_etag(StatusCode::OK, value))
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ListQuery {
    sort: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
    unassigned: Option<bool>,
    kind: Option<String>,
    album_id: Option<String>,
}

async fn list_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let actor = session_from(&state, &headers).await.map(|s| s.actor);
    Ok(Json(
        crate::album::list_images(
            &state.pool,
            actor.as_ref(),
            &album_id,
            q.sort.as_deref().unwrap_or("manual"),
            q.q.as_deref(),
            q.limit.unwrap_or(50),
            q.cursor.as_deref(),
        )
        .await?,
    ))
}

async fn get_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((album_id, image_id)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let actor = session_from(&state, &headers).await.map(|s| s.actor);
    let album = crate::album::get(&state.pool, actor.as_ref(), &album_id).await?;
    let order = match q.sort.as_deref().unwrap_or("manual") {
        "created_desc" => "i.created_at DESC, i.id DESC",
        "created_asc" => "i.created_at ASC, i.id ASC",
        "name_asc" => "COALESCE(ai.display_name, i.original_name) ASC, i.id ASC",
        _ => "ai.sort_key ASC, i.id ASC",
    };
    let sql = format!(
        "WITH ordered AS (
            SELECT i.id, COALESCE(ai.display_name, i.original_name) AS name, i.width, i.height,
              i.created_at, i.original_mime, i.revision, i.lifecycle_revision,
              lag(i.id) OVER (ORDER BY {order}) AS previous,
              lead(i.id) OVER (ORDER BY {order}) AS next
            FROM album_images ai JOIN images i ON i.id = ai.image_id
            WHERE ai.album_id = ? AND i.state = 'active'
              AND (? IS NULL OR i.original_name LIKE '%' || ? || '%' OR i.id = ?)
        ) SELECT name,width,height,created_at,original_mime,revision,lifecycle_revision,previous,next
           FROM ordered WHERE id = ?",
    );
    let (name, width, height, created, original_mime, revision, lifecycle, previous, next):
        (String, i64, i64, i64, String, i64, i64, Option<String>, Option<String>) =
        sqlx::query_as(&sql).bind(&album_id).bind(q.q.as_deref()).bind(q.q.as_deref())
            .bind(&image_id).bind(&image_id).fetch_optional(&state.pool).await
            .map_err(anyhow::Error::from)?.ok_or_else(AppError::not_found)?;
    let mut current = serde_json::json!({
        "id": image_id, "display_name": name, "width": width, "height": height,
        "created_at": crate::db::rfc3339(created), "revision": revision, "lifecycle_revision": lifecycle,
        "previous_image_id": previous, "next_image_id": next, "content_revision": album.content_revision,
    });
    current["can_inline_original"] = serde_json::json!(
        album.capabilities.view_original && crate::processing::browser_inline_safe(&original_mime)
    );
    let published = crate::versions::published_variants(&state.pool, &image_id)
        .await
        .map_err(anyhow::Error::from)?;
    current["variants"] = serde_json::json!(&published);
    current["selection"] = variant_selection(&published);
    current["srcset"] = srcset_json(&published);
    current["placeholder"] = serde_json::json!(crate::versions::detail_srcset(&published).is_empty());
    if let Some((source, text)) = sqlx::query_as::<_, (String, String)>(
        "SELECT source, text FROM captions WHERE image_id = ?
         ORDER BY CASE source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1",
    )
    .bind(&image_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    {
        current["caption"] = serde_json::json!({"source": source, "text": text});
    }
    Ok(Json(current))
}

fn byte_range(spec: &str, size: u64) -> Result<Option<(u64, u64)>, ()> {
    if size == 0 || spec.contains(',') {
        return Ok(None);
    }
    let spec = spec.strip_prefix("bytes=").ok_or(())?;
    if let Some(suffix) = spec.strip_prefix('-') {
        let n: u64 = suffix.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        let n = n.min(size);
        return Ok(Some((size - n, size - 1)));
    }
    let mut parts = spec.split('-');
    let start: u64 = parts.next().unwrap_or("").parse().map_err(|_| ())?;
    let end_raw = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err(());
    }
    if start >= size {
        return Err(());
    }
    let end = if end_raw.is_empty() {
        size - 1
    } else {
        let end: u64 = end_raw.parse().map_err(|_| ())?;
        end.min(size - 1)
    };
    if start > end {
        Err(())
    } else {
        Ok(Some((start, end)))
    }
}

async fn read_stored(
    state: &AppState,
    media: &crate::storage::Storage,
    blob_id: &str,
    key: &str,
    range: Option<(u64, u64)>,
) -> AppResult<Vec<u8>> {
    match media.backend().open(key, range).await {
        Ok(bytes) => Ok(bytes),
        Err(err) if crate::storage::is_missing(&err) => {
            let _ = crate::storage::mark_missing(&state.pool, &media.backend_id, blob_id).await;
            Err(AppError::new(
                StatusCode::NOT_FOUND,
                "MEDIA_MISSING",
                "Not Found",
                "stored object is missing",
            ))
        }
        Err(_) => Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
            "Unavailable",
            "storage read failed",
        )),
    }
}

async fn serve_blob(
    state: &AppState,
    blob_id: &str,
    download: bool,
    name: &str,
    mime: &str,
    headers: &HeaderMap,
    cache: &'static str,
) -> AppResult<Response> {
    let (backend_id, route_epoch) =
        crate::db::current_route(&state.pool).await.map_err(anyhow::Error::from)?;
    let media = crate::storage::open_by_id(&state.pool, &backend_id)
        .await
        .map_err(anyhow::Error::from)?;
    let (key, size, sha, stored_mime) =
        crate::storage::location_for(&state.pool, blob_id, &media.backend_id)
            .await
            .map_err(|_| AppError::not_found())?;
    let etag = format!("\"{sha}\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &etag)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CACHE_CONTROL, cache)
            .body(Body::empty())
            .unwrap());
    }
    let if_range = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok());
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let ranged = if if_range.is_some_and(|value| value != etag) {
        None
    } else {
        range_header
    };
    let selected = if let Some(spec) = ranged {
        match byte_range(spec, size as u64) {
            Ok(Some((start, end))) => Some((start, end)),
            Ok(None) => None,
            Err(()) => {
                return Err(AppError::new(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "RANGE",
                    "Range Not Satisfiable",
                    "invalid range",
                ));
            }
        }
    } else {
        None
    };
    let pin = crate::storage::pin_media_read(&state.pool, &media.backend_id, route_epoch, blob_id)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(path) = media.backend().local_file(&key) {
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(file) => match file.metadata().await {
                Ok(meta) if meta.is_file() => file,
                _ => {
                    return Err(AppError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "STORAGE_UNAVAILABLE",
                        "Unavailable",
                        "storage read failed",
                    ));
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let _ = crate::storage::mark_missing(&state.pool, &media.backend_id, blob_id).await;
                return Err(AppError::new(
                    StatusCode::NOT_FOUND,
                    "MEDIA_MISSING",
                    "Not Found",
                    "stored object is missing",
                ));
            }
            Err(_) => {
                return Err(AppError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "STORAGE_UNAVAILABLE",
                    "Unavailable",
                    "storage read failed",
                ));
            }
        };
        let (status, content_range, length, max_bytes) = if let Some((start, end)) = selected {
            use tokio::io::{AsyncSeekExt, SeekFrom};
            if file.seek(SeekFrom::Start(start)).await.is_err() {
                return Err(AppError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "STORAGE_UNAVAILABLE",
                    "Unavailable",
                    "storage read failed",
                ));
            }
            let length = end - start + 1;
            (
                StatusCode::PARTIAL_CONTENT,
                Some(format!("bytes {start}-{end}/{size}")),
                length,
                Some(length),
            )
        } else {
            (StatusCode::OK, None, size as u64, None)
        };
        let body = crate::http_util::stream_open_file(file, max_bytes, None);
        let mut builder = Response::builder()
            .status(status)
            .header(
                header::CONTENT_TYPE,
                if mime.is_empty() { stored_mime } else { mime.to_string() },
            )
            .header(header::CONTENT_LENGTH, length)
            .header(header::ETAG, etag)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(header::CACHE_CONTROL, cache);
        if download {
            builder = builder.header(
                header::CONTENT_DISPOSITION,
                crate::http_util::content_disposition(name),
            );
        }
        if let Some(cr) = content_range {
            builder = builder.header(header::CONTENT_RANGE, cr);
        }
        return Ok(builder.body(hold_read(body, pin)).unwrap());
    }
    if size <= 0 {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, 0)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache)
            .body(hold_read(Body::empty(), pin))
            .unwrap());
    }
    let (start, end) = selected.unwrap_or((0, size as u64 - 1));
    let length = end - start + 1;
    let status = if selected.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, if mime.is_empty() { stored_mime } else { mime.to_string() })
        .header(header::CONTENT_LENGTH, length)
        .header(header::ETAG, etag)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, cache);
    if download {
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            crate::http_util::content_disposition(name),
        );
    }
    if selected.is_some() {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    let body = stream_remote_object(state, media, blob_id, key, start, end).await?;
    Ok(builder.body(hold_read(body, pin)).unwrap())
}

const REMOTE_CHUNK: u64 = 64 * 1024;

async fn stream_remote_object(
    state: &AppState,
    media: crate::storage::Storage,
    blob_id: &str,
    key: String,
    start: u64,
    end: u64,
) -> AppResult<Body> {
    let first_end = (start + REMOTE_CHUNK - 1).min(end);
    let first = read_stored(state, &media, blob_id, &key, Some((start, first_end))).await?;
    let next = first_end + 1;
    let rest = stream::unfold((next <= end).then_some(next), move |cursor| {
        let media = media.clone();
        let key = key.clone();
        async move {
            let offset = cursor?;
            let chunk_end = (offset + REMOTE_CHUNK - 1).min(end);
            let read = media.backend().open(&key, Some((offset, chunk_end))).await;
            let following = (chunk_end + 1 <= end).then_some(chunk_end + 1);
            match read {
                Ok(bytes) => Some((Ok::<_, std::io::Error>(bytes::Bytes::from(bytes)), following)),
                Err(err) => Some((Err(std::io::Error::other(err)), None)),
            }
        }
    });
    let stream = stream::once(async move { Ok(bytes::Bytes::from(first)) }).chain(rest);
    Ok(Body::from_stream(stream))
}

fn hold_read(body: Body, pin: crate::storage::MediaRead) -> Body {
    let stream = body.into_data_stream().map(move |item| {
        let _pin = &pin;
        item
    });
    Body::from_stream(stream)
}

async fn variant_body(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((album_id, image_id, variant_id)): Path<(String, String, String)>,
) -> AppResult<Response> {
    let actor = session_from(&state, &headers).await.map(|s| s.actor);
    let album = crate::album::get(&state.pool, actor.as_ref(), &album_id).await?;
    if !album.capabilities.browse {
        return Err(AppError::not_found());
    }
    crate::album::image_in_album(&state.pool, &album_id, &image_id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(AppError::not_found)?;
    let blob = readable_variant_blob(&state, &image_id, &variant_id).await?;
    serve_blob(
        &state,
        &blob,
        false,
        "",
        "",
        &headers,
        "private, no-cache",
    )
    .await
}

async fn original_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((album_id, image_id)): Path<(String, String)>,
) -> AppResult<Response> {
    original_bytes(&state, &headers, &album_id, &image_id, false).await
}

async fn original_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((album_id, image_id)): Path<(String, String)>,
) -> AppResult<Response> {
    original_bytes(&state, &headers, &album_id, &image_id, true).await
}

async fn original_bytes(
    state: &AppState,
    headers: &HeaderMap,
    album_id: &str,
    image_id: &str,
    download: bool,
) -> AppResult<Response> {
    let actor = session_from(state, headers).await.map(|s| s.actor);
    let album = crate::album::get(&state.pool, actor.as_ref(), album_id).await?;
    if download && !album.capabilities.download {
        return Err(AppError::not_found());
    }
    if !download && !album.capabilities.view_original {
        return Err(AppError::not_found());
    }
    let img = crate::album::image_in_album(&state.pool, album_id, image_id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(AppError::not_found)?;
    if !crate::processing::browser_inline_safe(&img.3) {
        if download {
            return serve_blob(
                state,
                &img.5,
                true,
                &img.0,
                "application/octet-stream",
                headers,
                "private, no-store",
            )
            .await;
        }
        let variants = crate::versions::published_variants(&state.pool, image_id)
            .await
            .map_err(anyhow::Error::from)?;
        let variant = crate::versions::select_for_spec(&variants, "thumb").map(|(variant, _)| variant);
        let Some(variant) = variant else {
            return Err(AppError::new(
                StatusCode::NOT_FOUND,
                "INLINE_UNSAFE",
                "Not Found",
                "original cannot be rendered in the browser",
            ));
        };
        let blob = readable_variant_blob(state, image_id, &variant.id).await?;
        return serve_blob(
            state,
            &blob,
            false,
            "",
            "",
            headers,
            "private, no-cache",
        )
        .await;
    }
    serve_blob(
        state,
        &img.5,
        download,
        &img.0,
        &img.3,
        headers,
        "private, no-store",
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdsBody {
    #[serde(default)]
    image_ids: Vec<String>,
    expected_revision: Option<i64>,
    target_album_id: Option<String>,
    target_expected_revision: Option<i64>,
    before_id: Option<String>,
    after_id: Option<String>,
    image_id: Option<String>,
}

async fn items_rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let image_id = body["image_id"].as_str().unwrap_or("");
    let display_name = body["display_name"].as_str().unwrap_or("");
    if image_id.is_empty() {
        return Err(AppError::bad_request("IMAGE_ID", "image_id required"));
    }
    let revision = resolve_revision(&headers, body["expected_revision"].as_i64())?;
    Ok(Json(
        crate::album::rename_display(
            &state.pool,
            &s.actor,
            &album_id,
            image_id,
            display_name,
            revision,
        )
        .await?,
    ))
}

async fn items_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<IdsBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let revision = resolve_revision(&headers, body.expected_revision)?;
    Ok(Json(
        crate::album::add_images(&state.pool, &s.actor, &album_id, &body.image_ids, Some(revision))
            .await?,
    ))
}

async fn items_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<IdsBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let revision = resolve_revision(&headers, body.expected_revision)?;
    Ok(Json(
        crate::album::remove_images(&state.pool, &s.actor, &album_id, &body.image_ids, Some(revision))
            .await?,
    ))
}

async fn items_move(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<IdsBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let target = body.target_album_id.ok_or_else(|| AppError::bad_request("MISSING", "target_album_id"))?;
    let revision = resolve_revision(&headers, body.expected_revision)?;
    let target_revision = body.target_expected_revision.ok_or_else(|| {
        AppError::new(
            StatusCode::PRECONDITION_REQUIRED,
            "REVISION_REQUIRED",
            "Precondition Required",
            "target_expected_revision required",
        )
    })?;
    Ok(Json(
        crate::album::move_images(
            &state.pool,
            &s.actor,
            &album_id,
            &target,
            &body.image_ids,
            Some(revision),
            Some(target_revision),
            None,
        )
        .await?,
    ))
}

async fn items_reorder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<IdsBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let image_id = body
        .image_id
        .or_else(|| body.image_ids.first().cloned())
        .ok_or_else(|| AppError::bad_request("MISSING", "image_id"))?;
    let revision = resolve_revision(&headers, body.expected_revision)?;
    Ok(Json(
        crate::album::reorder(
            &state.pool,
            &s.actor,
            &album_id,
            &image_id,
            body.before_id.as_deref(),
            body.after_id.as_deref(),
            Some(revision),
            None,
        )
        .await?,
    ))
}

async fn create_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<crate::upload::CreateBatch>,
) -> AppResult<(StatusCode, Json<crate::upload::BatchCreated>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok((StatusCode::CREATED, Json(crate::upload::create_batch(&state.pool, &s.actor, &album_id, body).await?)))
}

async fn upload_body(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((batch_id, item_id)): Path<(String, String)>,
    request: Request,
) -> AppResult<(StatusCode, Json<crate::upload::CommitResult>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    crate::upload::begin_receive(&state.pool, &s.actor, &batch_id, &item_id).await?;
    let body = match crate::http_util::read_with_idle(
        request.into_body(),
        100 * 1024 * 1024,
        crate::http_util::READ_IDLE,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(crate::http_util::ReadFault::Idle) => {
            return Err(AppError::new(
                StatusCode::REQUEST_TIMEOUT,
                "READ_IDLE",
                "Request Timeout",
                "upload stalled",
            ))
        }
        Err(crate::http_util::ReadFault::TooLarge) => {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "UPLOAD_TOO_LARGE",
                "Payload Too Large",
                "file exceeds 100 MiB",
            ))
        }
        Err(crate::http_util::ReadFault::Closed) => {
            return Err(AppError::bad_request("BODY", "could not read upload"))
        }
    };
    let (status, result) = crate::upload::put_item(&state, &s.actor, &batch_id, &item_id, &body).await?;
    Ok((StatusCode::from_u16(status).unwrap_or(StatusCode::CREATED), Json(result)))
}

async fn list_uploads(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::upload::list_batches(&state.pool, &s.actor).await?))
}

async fn get_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::upload::get_batch(&state.pool, &s.actor, &batch_id).await?))
}

async fn cancel_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let (status, value) = crate::upload::cancel_batch(&state.pool, &s.actor, &batch_id).await?;
    Ok((StatusCode::from_u16(status).unwrap_or(StatusCode::OK), Json(value)))
}

async fn selected_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
    Json(body): Json<IdsBody>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let payload = serde_json::json!({ "image_ids": body.image_ids });
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        format!("/albums/{album_id}/downloads/selected"),
        &headers,
        payload,
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::download::selected(&state.pool, &s.actor, &album_id, &body.image_ids).await?;
    cmd.remember(&state.pool, 200, &value).await?;
    Ok((StatusCode::OK, Json(value)).into_response())
}

async fn claim_body(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(claim_id): Path<String>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    let (album_id, image_id, _) = crate::download::claim_meta(&state.pool, &s.actor, &claim_id).await?;
    original_bytes(&state, &headers, &album_id, &image_id, true).await
}

fn desktop_archive_client(headers: &HeaderMap) -> bool {
    headers
        .get("x-gallery-client")
        .and_then(|value| value.to_str().ok())
        == Some("desktop")
}

async fn create_archive(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        format!("/albums/{album_id}/archives"),
        &headers,
        serde_json::json!({}),
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    if !desktop_archive_client(&headers) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "ARCHIVE_CLIENT",
            "Forbidden",
            "original archives are only offered to the desktop client",
        ));
    }
    let (status, body) = crate::download::request_archive(&state, &s.actor, &album_id).await?;
    cmd.remember(&state.pool, status, &body).await?;
    Ok((StatusCode::from_u16(status).unwrap(), Json(body)).into_response())
}

async fn list_archives(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::download::list_archives(&state.pool, &s.actor).await.map_err(anyhow::Error::from)?))
}

async fn get_archive(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    let _ = crate::download::expire_archives(&state.pool, &state.storage).await;
    Ok(Json(crate::download::archive_status(&state.pool, &s.actor, &id).await?))
}

async fn archive_body(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    if !crate::download::hold_archive(&state.pool, &id, &s.actor.id).await? {
        let current = sqlx::query_as::<_, (String,)>(
            "SELECT archive_state FROM archive_packages WHERE id = ?",
        )
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
        if current.as_ref().is_none_or(|row| row.0 == "ready") {
            let _ = crate::download::expire_archives(&state.pool, &state.storage).await;
            return Err(AppError::not_found());
        }
    }
    let meta = crate::download::archive_status(&state.pool, &s.actor, &id).await?;
    if meta["state"] == "stale" {
        return Err(AppError::conflict("ARCHIVE_STALE", "snapshot stale"));
    }
    if meta["state"] != "ready" {
        return Err(AppError::conflict("ARCHIVE_NOT_READY", "archive not ready"));
    }
    let row = sqlx::query_as::<_, (Option<String>, i64, i64, String)>(
        "SELECT blob_id, content_revision, acl_revision, album_id FROM archive_packages WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let album = crate::album::get(&state.pool, Some(&s.actor), &row.3).await?;
    if !album.capabilities.download || !album.capabilities.archive_enabled {
        return Err(AppError::new(StatusCode::FORBIDDEN, "ARCHIVE_DISABLED", "Forbidden", "disabled"));
    }
    if album.content_revision != row.1 || album.acl_revision != row.2 {
        return Err(AppError::conflict("ARCHIVE_STALE", "snapshot stale"));
    }
    let blob = row.0.ok_or_else(AppError::not_found)?;
    let mut response = serve_blob(
        &state,
        &blob,
        true,
        "album.zip",
        "application/zip",
        &headers,
        "private, no-store",
    )
    .await?;
    let hold = crate::download::ArchiveHold::new(state.pool.clone(), id, s.actor.id.clone());
    let current = std::mem::replace(response.body_mut(), Body::empty());
    let stream = current.into_data_stream().map(move |item| {
        let _hold = &hold;
        item
    });
    *response.body_mut() = Body::from_stream(stream);
    Ok(response)
}

async fn restore_album(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(album_id): Path<String>,
) -> AppResult<Json<crate::album::AlbumView>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let revision = resolve_revision(&headers, None)?;
    Ok(Json(
        crate::album::restore_album(&state.pool, &s.actor, &album_id, revision).await?,
    ))
}

async fn list_jobs(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    let items = crate::jobs::list(&state.pool, Some(&s.actor.id), s.actor.role == "owner", 50)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::json!({"items": items, "next_cursor": null})))
}

async fn visible_job(state: &AppState, actor: &Actor, id: &str) -> AppResult<crate::jobs::JobView> {
    let job = crate::jobs::get(&state.pool, id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(AppError::not_found)?;
    if actor.role != "owner" {
        let owner = sqlx::query_as::<_, (Option<String>,)>("SELECT actor_id FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
        match owner {
            Some((Some(aid),)) if aid == actor.id => {}
            _ => return Err(AppError::not_found()),
        }
    }
    Ok(job)
}

async fn get_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<crate::jobs::JobView>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(visible_job(&state, &s.actor, &id).await?))
}

async fn cancel_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<crate::jobs::JobView>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let before = visible_job(&state, &s.actor, &id).await?;
    let job = crate::jobs::cancel(&state.pool, &id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(AppError::not_found)?;
    if job.kind == "scan_plan" {
        crate::ai::cancel_scan_parent(&state.pool, &id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let status = if matches!(before.state.as_str(), "succeeded" | "failed" | "canceled") {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(job)))
}

async fn retry_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<crate::jobs::JobView>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let before = visible_job(&state, &s.actor, &id).await?;
    if !matches!(before.state.as_str(), "failed" | "canceled") {
        return Ok((StatusCode::OK, Json(before)));
    }
    let job = crate::jobs::retry(&state.pool, &id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(AppError::not_found)?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn audit_export(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    let body = crate::events::export_audit_jsonl(&state.pool, &s.actor).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(body))
        .unwrap())
}

async fn capabilities(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let limits = crate::settings::limits(&state.pool).await?;
    Ok(Json(serde_json::json!({
        "read": ["jpeg","png","gif","webp","bmp","tiff"],
        "write": ["png","jpeg","webp"],
        "animation": false,
        "multipage": false,
        "lossy": ["jpeg"],
        "max_upload_items": limits.max_upload_items,
        "upload_concurrency_hint": limits.upload_concurrency_hint,
        "max_download_selection": limits.max_download_selection,
        "ai_enabled": state.config.ai_enabled
    })))
}

async fn get_profile(State(state): State<AppState>) -> AppResult<Response> {
    let (rev, specs) = crate::versions::current_profile(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(with_revision_etag(
        StatusCode::OK,
        serde_json::json!({"revision": rev, "specs": specs}),
    ))
}

async fn put_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(specs): Json<Vec<crate::versions::VariantSpec>>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    auth::require_owner(&s.actor)?;
    if specs.is_empty() {
        return Err(AppError::unprocessable("PROFILE", "at least one variant spec"));
    }
    let expected = resolve_revision(&headers, None)?;
    let current = sqlx::query_as::<_, (i64, String)>(
        "SELECT revision, config_hash FROM variant_profiles ORDER BY revision DESC LIMIT 1",
    )
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if current.0 != expected {
        return Err(AppError::precondition("REVISION_CONFLICT", "profile revision"));
    }
    let json = serde_json::to_string(&specs).map_err(anyhow::Error::from)?;
    let hash = sha256_hex(json.as_bytes());
    if hash == current.1 {
        return Ok(with_revision_etag(
            StatusCode::OK,
            serde_json::json!({"revision": current.0, "job_ids": []}),
        ));
    }
    let mut tx = crate::db::begin_immediate(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let next = current.0 + 1;
    sqlx::query(
        "INSERT INTO variant_profiles(profile_id, revision, specs_json, config_hash, created_at)
         VALUES('default', ?, ?, ?, ?)",
    )
    .bind(next)
    .bind(&json)
    .bind(&hash)
    .bind(crate::db::now_ms())
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let images = sqlx::query_as::<_, (String,)>("SELECT id FROM images WHERE state = 'active'")
        .fetch_all(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let mut job_ids = Vec::new();
    for (image_id,) in images {
        let planned = crate::versions::plan_variants(&mut tx, &image_id, Some(&s.actor.id), None)
            .await
            .map_err(anyhow::Error::from)?;
        job_ids.extend(planned);
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    Ok(with_revision_etag(
        StatusCode::ACCEPTED,
        serde_json::json!({"revision": next, "job_ids": job_ids}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversionIn {
    image_id: String,
    spec: crate::versions::VariantSpec,
}

async fn create_conversion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ConversionIn>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    source_image_visible(&state, &s.actor, &body.image_id).await?;
    let id = crate::ids::new_id();
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO conversions(id, actor_id, source_image_id, spec_json, state, created_at)
         VALUES(?, ?, ?, ?, 'queued', ?)",
    )
    .bind(&id)
    .bind(&s.actor.id)
    .bind(&body.image_id)
    .bind(serde_json::to_string(&body.spec).unwrap())
    .bind(crate::db::now_ms())
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let payload = crate::jobs::JobPayload {
        kind: "convert_image".into(),
        payload_version: 1,
        scope: serde_json::json!({"conversion_id": id, "image_id": body.image_id}),
        target: serde_json::json!({}),
        actor_id: Some(s.actor.id.clone()),
    };
    let job_id = crate::jobs::enqueue(&mut tx, "convert_image", &format!("convert:{id}"), &payload, Some(&s.actor.id), 50)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE conversions SET job_id = ? WHERE id = ?")
        .bind(&job_id)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"id": id, "job_id": job_id}))))
}

async fn get_conversion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    let row = sqlx::query_as::<_, (String, String, Option<String>, Option<i64>)>(
        "SELECT actor_id, state, output_blob_id, expires_at FROM conversions WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != s.actor.id {
        return Err(AppError::not_found());
    }
    let expired = row.3.is_some_and(|exp| exp < crate::db::now_ms());
    let state_name = if expired && row.1 == "ready" { "expired" } else { row.1.as_str() };
    Ok(Json(serde_json::json!({
        "id": id,
        "state": state_name,
        "output_id": if expired { None } else { row.2 }
    })))
}

async fn source_image_visible(state: &AppState, actor: &Actor, image_id: &str) -> AppResult<()> {
    let image = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM images WHERE id = ? AND state = 'active'",
    )
    .bind(image_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if image.is_none() {
        return Err(AppError::not_found());
    }
    if actor.role == "owner" {
        return Ok(());
    }
    let albums = sqlx::query_as::<_, (String,)>(
        "SELECT ai.album_id FROM album_images ai
         JOIN albums a ON a.id = ai.album_id
         WHERE ai.image_id = ? AND a.deleted_at IS NULL",
    )
    .bind(image_id)
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    for (album_id,) in albums {
        if let Ok(album) = crate::album::get(&state.pool, Some(actor), &album_id).await {
            if album.capabilities.view_original {
                return Ok(());
            }
        }
    }
    Err(AppError::not_found())
}

async fn conversion_body(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, output_id)): Path<(String, String)>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    let row = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
        "SELECT actor_id, output_blob_id, expires_at FROM conversions WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != s.actor.id {
        return Err(AppError::not_found());
    }
    if row.2.is_some_and(|exp| exp < crate::db::now_ms()) {
        return Err(AppError::new(
            StatusCode::GONE,
            "CONVERSION_EXPIRED",
            "Gone",
            "conversion output expired",
        ));
    }
    let blob = row.1.ok_or_else(AppError::not_found)?;
    if blob != output_id {
        return Err(AppError::not_found());
    }
    serve_blob(
        &state,
        &blob,
        true,
        "conversion.bin",
        "application/octet-stream",
        &headers,
        "private, no-store",
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptionBody {
    text: String,
    expected_revision: Option<i64>,
}

async fn patch_caption(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
    Json(body): Json<CaptionBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let expected = resolve_revision(&headers, body.expected_revision)?;
    let chars = body.text.chars().count();
    if chars == 0 || chars > 2000 {
        return Err(AppError::unprocessable("CAPTION", "caption length"));
    }
    if !image_editable(&state, &s.actor, &image_id).await? {
        return Err(AppError::not_found());
    }
    let mut tx = crate::db::begin_immediate(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let current = sqlx::query_as::<_, (String, i64)>(
        "SELECT state, revision FROM images WHERE id = ?",
    )
    .bind(&image_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if current.0 != "active" {
        return Err(AppError::not_found());
    }
    if current.1 != expected {
        return Err(AppError::precondition("REVISION_CONFLICT", "image revision"));
    }
    let now = crate::db::now_ms();
    sqlx::query(
        "INSERT INTO captions(image_id, source, text, updated_at) VALUES(?, 'human', ?, ?)
         ON CONFLICT(image_id, source) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
    )
    .bind(&image_id)
    .bind(&body.text)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE images SET revision = revision + 1 WHERE id = ? AND revision = ?")
        .bind(&image_id)
        .bind(expected)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let (revision,): (i64,) = sqlx::query_as("SELECT revision FROM images WHERE id = ?")
        .bind(&image_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    if revision == expected {
        return Err(AppError::precondition("REVISION_CONFLICT", "image revision"));
    }
    crate::search::sync_image(&mut tx, &image_id)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::json!({
        "image_id": image_id,
        "text": body.text,
        "source": "human",
        "revision": revision
    })))
}

async fn image_editable(state: &AppState, actor: &Actor, image_id: &str) -> AppResult<bool> {
    let image = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM images WHERE id = ? AND state = 'active'",
    )
    .bind(image_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if image.is_none() {
        return Ok(false);
    }
    if actor.role == "owner" {
        return Ok(true);
    }
    let albums = sqlx::query_as::<_, (String,)>(
        "SELECT ai.album_id FROM album_images ai
         JOIN albums a ON a.id = ai.album_id
         WHERE ai.image_id = ? AND a.deleted_at IS NULL",
    )
    .bind(image_id)
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    for (album_id,) in albums {
        if let Ok(album) = crate::album::get(&state.pool, Some(actor), &album_id).await {
            if album.capabilities.edit {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleBody {
    expected_lifecycle_revision: Option<i64>,
}

fn require_lifecycle(body: &LifecycleBody) -> AppResult<i64> {
    body.expected_lifecycle_revision.ok_or_else(|| {
        AppError::new(
            StatusCode::PRECONDITION_REQUIRED,
            "LIFECYCLE_REQUIRED",
            "Precondition Required",
            "expected_lifecycle_revision required",
        )
    })
}

fn owner_or_hidden(actor: &Actor) -> AppResult<()> {
    if actor.role == "owner" {
        Ok(())
    } else {
        Err(AppError::not_found())
    }
}

async fn trash_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
    Json(body): Json<LifecycleBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    owner_or_hidden(&s.actor)?;
    let (state_name, revision) = crate::album::transition_lifecycle(
        &state.pool,
        Some(&s.actor),
        &image_id,
        require_lifecycle(&body)?,
        "trashed",
    )
    .await?;
    Ok(Json(serde_json::json!({"id": image_id, "state": state_name, "lifecycle_revision": revision})))
}

async fn restore_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
    Json(body): Json<LifecycleBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    owner_or_hidden(&s.actor)?;
    let (state_name, revision) = crate::album::transition_lifecycle(
        &state.pool,
        Some(&s.actor),
        &image_id,
        require_lifecycle(&body)?,
        "active",
    )
    .await?;
    Ok(Json(serde_json::json!({"id": image_id, "state": state_name, "lifecycle_revision": revision})))
}

async fn purge_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
    Json(body): Json<LifecycleBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    owner_or_hidden(&s.actor)?;
    let (state_name, revision) = crate::album::transition_lifecycle(
        &state.pool,
        Some(&s.actor),
        &image_id,
        require_lifecycle(&body)?,
        "purging",
    )
    .await?;
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    let payload = crate::jobs::JobPayload {
        kind: "purge_media".into(),
        payload_version: 1,
        scope: serde_json::json!({"image_id": image_id}),
        target: serde_json::json!({}),
        actor_id: Some(s.actor.id.clone()),
    };
    let job_id = crate::jobs::enqueue(
        &mut tx,
        "purge_media",
        &format!("purge:{image_id}"),
        &payload,
        Some(&s.actor.id),
        5,
    )
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    let status = if state_name == "purging" && revision == require_lifecycle(&body)? {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((
        status,
        Json(serde_json::json!({
            "id": image_id,
            "state": state_name,
            "lifecycle_revision": revision,
            "job_id": job_id
        })),
    ))
}

async fn library_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    if s.actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let unassigned = q.unassigned.unwrap_or(false);
    let sql = if unassigned {
        "SELECT i.id, i.original_name, i.width, i.height, i.created_at FROM images i
         WHERE i.state = 'active' AND NOT EXISTS (
            SELECT 1 FROM album_images ai JOIN albums a ON a.id = ai.album_id
            WHERE ai.image_id = i.id AND a.deleted_at IS NULL)
         ORDER BY i.created_at DESC LIMIT 50"
    } else {
        "SELECT i.id, i.original_name, i.width, i.height, i.created_at FROM images i
         WHERE i.state = 'active' ORDER BY i.created_at DESC LIMIT 50"
    };
    let rows = sqlx::query_as::<_, (String, String, i64, i64, i64)>(sql)
        .fetch_all(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::json!({
        "items": rows.into_iter().map(|(id, name, w, h, c)| serde_json::json!({
            "id": id, "display_name": name, "width": w, "height": h, "created_at": crate::db::rfc3339(c)
        })).collect::<Vec<_>>(),
        "next_cursor": null
    })))
}

async fn library_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    if s.actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let row = sqlx::query_as::<_, (String, i64, i64, i64, i64, String, String)>(
        "SELECT original_name, width, height, revision, lifecycle_revision, original_mime, state FROM images WHERE id = ?",
    )
    .bind(&image_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let variants = crate::versions::published_variants(&state.pool, &image_id)
        .await
        .map_err(anyhow::Error::from)?;
    let caption: Option<(String, String)> = sqlx::query_as(
        "SELECT source,text FROM captions WHERE image_id = ?
         ORDER BY CASE source WHEN 'human' THEN 0 ELSE 1 END LIMIT 1",
    ).bind(&image_id).fetch_optional(&state.pool).await.map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::json!({
        "id": image_id,
        "display_name": row.0,
        "width": row.1,
        "height": row.2,
        "revision": row.3,
        "lifecycle_revision": row.4,
        "can_inline_original": row.6 == "active" && crate::processing::browser_inline_safe(&row.5),
        "caption": caption.map(|(source, text)| serde_json::json!({"source": source, "text": text})),
        "variants": variants,
        "selection": variant_selection(&variants),
        "srcset": srcset_json(&variants),
        "placeholder": crate::versions::detail_srcset(&variants).is_empty()
    })))
}

fn srcset_json(variants: &[crate::versions::PublishedVariant]) -> serde_json::Value {
    serde_json::json!(
        crate::versions::detail_srcset(variants)
            .into_iter()
            .map(|variant| serde_json::json!({
                "variant_id": variant.id,
                "spec_id": variant.spec_id,
                "width": variant.width,
                "height": variant.height
            }))
            .collect::<Vec<_>>()
    )
}

fn variant_selection(variants: &[crate::versions::PublishedVariant]) -> serde_json::Value {
    let mut selection = serde_json::Map::new();
    for spec in ["thumb", "mobile", "tablet", "desktop"] {
        selection.insert(
            spec.to_string(),
            match crate::versions::select_for_spec(variants, spec) {
                Some((variant, fallback)) => serde_json::json!({
                    "variant_id": variant.id,
                    "spec_id": variant.spec_id,
                    "fallback": fallback
                }),
                None => serde_json::Value::Null,
            },
        );
    }
    serde_json::Value::Object(selection)
}

async fn owner_unassigned_image(
    state: &AppState,
    headers: &HeaderMap,
    image_id: &str,
) -> AppResult<(String, String, String)> {
    let s = require_user(state, headers).await?;
    if s.actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT i.original_blob_id, i.original_name, i.original_mime
         FROM images i
         WHERE i.id = ? AND i.state = 'active'
           AND NOT EXISTS (
             SELECT 1 FROM album_images ai
             JOIN albums a ON a.id = ai.album_id
             WHERE ai.image_id = i.id AND a.deleted_at IS NULL
           )",
    )
    .bind(image_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    Ok(row)
}

async fn readable_variant_blob(state: &AppState, image_id: &str, variant_id: &str) -> AppResult<String> {
    let row = sqlx::query_as::<_, (Option<String>, String, String, Option<i64>)>(
        "SELECT blob_id, state, spec_id, retain_until FROM image_variants WHERE id = ? AND image_id = ?",
    )
    .bind(variant_id)
    .bind(image_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let Some(blob_id) = row.0 else {
        return Err(AppError::not_found());
    };
    if row.1 != "published" {
        return Err(AppError::not_found());
    }
    let head = sqlx::query_as::<_, (String,)>(
        "SELECT variant_id FROM variant_heads WHERE image_id = ? AND spec_id = ?",
    )
    .bind(image_id)
    .bind(&row.2)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if head.as_ref().map(|item| item.0.as_str()) == Some(variant_id) {
        return Ok(blob_id);
    }
    if row.3.is_some_and(|until| until > crate::db::now_ms()) {
        return Ok(blob_id);
    }
    sqlx::query("UPDATE image_variants SET state = 'superseded' WHERE id = ? AND state = 'published'")
        .bind(variant_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    Err(AppError::not_found())
}

async fn library_variant_body(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((image_id, variant_id)): Path<(String, String)>,
) -> AppResult<Response> {
    owner_unassigned_image(&state, &headers, &image_id).await?;
    let blob = readable_variant_blob(&state, &image_id, &variant_id).await?;
    serve_blob(
        &state,
        &blob,
        false,
        "",
        "",
        &headers,
        "private, no-cache",
    )
    .await
}

async fn library_original_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
) -> AppResult<Response> {
    let (blob, name, mime) = owner_unassigned_image(&state, &headers, &image_id).await?;
    serve_blob(&state, &blob, false, &name, &mime, &headers, "private, no-store").await
}

async fn library_original_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
) -> AppResult<Response> {
    let (blob, name, mime) = owner_unassigned_image(&state, &headers, &image_id).await?;
    serve_blob(&state, &blob, true, &name, &mime, &headers, "private, no-store").await
}

async fn list_trash(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    if s.actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let rows = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT id, original_name, lifecycle_revision FROM images WHERE state = 'trashed' ORDER BY deleted_at DESC",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(Json(serde_json::json!({
        "items": rows.into_iter().map(|(id, name, lifecycle_revision)| serde_json::json!({"id": id, "display_name": name, "lifecycle_revision": lifecycle_revision})).collect::<Vec<_>>()
    })))
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let actor = session_from(&state, &headers).await.map(|s| s.actor);
    Ok(Json(
        crate::search::query(
            &state.pool,
            actor.as_ref(),
            q.q.as_deref().unwrap_or(""),
            q.kind.as_deref(),
            q.album_id.as_deref(),
            q.limit.unwrap_or(50),
            q.cursor.as_deref(),
        )
        .await?,
    ))
}

async fn ai_scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<crate::ai::ScanRequest>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let payload = serde_json::to_value(&body).map_err(anyhow::Error::from)?;
    let cmd = crate::idempotency::Command::prepare(&s.actor.id, "POST", "/ai/scans", &headers, payload);
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let view = crate::ai::start_scan(&state, &s.actor, body).await?;
    let value = serde_json::to_value(&view).map_err(anyhow::Error::from)?;
    cmd.remember(&state.pool, 202, &value).await?;
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

async fn get_scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::ai::get_scan(&state.pool, &s.actor, &id).await?))
}

async fn cancel_scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    Ok(Json(crate::ai::cancel_scan(&state.pool, &s.actor, &id).await?))
}

async fn list_agent(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::ai::list_sessions(&state.pool, &s.actor).await?))
}

async fn create_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<crate::ai::CreateSession>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let payload = serde_json::to_value(&body).map_err(anyhow::Error::from)?;
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        "/agent/sessions",
        &headers,
        payload,
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::ai::create_session(&state.pool, &s.actor, state.config.ai_enabled, body).await?;
    cmd.remember(&state.pool, 201, &value).await?;
    Ok((StatusCode::CREATED, Json(value)).into_response())
}

async fn get_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    Ok(Json(crate::ai::get_session(&state.pool, &s.actor, &id).await?))
}

async fn agent_dispatch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let session = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &session)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    let result = crate::ai::dispatch_queued_message(&state.pool, &session.actor, &id).await?;
    Ok((StatusCode::ACCEPTED, Json(result)))
}

async fn agent_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    Ok(Json(crate::ai::list_messages(&state.pool, &s.actor, &id).await?))
}

async fn agent_post_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<crate::ai::IncomingMessage>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    let (status, value) = crate::ai::post_message(&state.pool, &s.actor, &id, body).await?;
    Ok((StatusCode::from_u16(status).unwrap_or(StatusCode::ACCEPTED), Json(value)))
}

async fn agent_remove_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, message_id)): Path<(String, String)>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    Ok(Json(crate::ai::remove_message(&state.pool, &s.actor, &id, &message_id).await?))
}

async fn agent_stop_turn(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, turn_id)): Path<(String, String)>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        format!("/agent/sessions/{id}/turns/{turn_id}/stop"),
        &headers,
        serde_json::json!({}),
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::ai::stop_turn(&state.pool, &s.actor, &id, &turn_id).await?;
    let status = if value["state"] == "stop_requested" { 202 } else { 200 };
    cmd.remember(&state.pool, status, &value).await?;
    Ok((StatusCode::from_u16(status).unwrap(), Json(value)).into_response())
}

async fn agent_resend_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, message_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    let client_message_id = body["client_message_id"].as_str().unwrap_or("");
    Ok(Json(
        crate::ai::resend_unknown(&state.pool, &s.actor, &id, &message_id, client_message_id).await?,
    ))
}

async fn agent_resume_pending(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    Ok(Json(crate::ai::resume_pending(&state.pool, &s.actor, &id).await?))
}

async fn agent_archive(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RevisionBody>,
) -> AppResult<Json<serde_json::Value>> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    Ok(Json(
        crate::ai::archive_session(&state.pool, &s.actor, &id, body.expected_revision).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionBody {
    expected_revision: Option<i64>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ActionBody {
    expected_revision: i64,
}

async fn agent_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, action_id)): Path<(String, String)>,
    Json(body): Json<ActionBody>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    let payload = serde_json::to_value(&body).map_err(anyhow::Error::from)?;
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        format!("/agent/sessions/{id}/actions/{action_id}/approve"),
        &headers,
        payload,
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::ai::approve_action(&state.pool, &s.actor, &id, &action_id, body.expected_revision).await?;
    cmd.remember(&state.pool, 200, &value).await?;
    Ok((StatusCode::OK, Json(value)).into_response())
}

async fn agent_reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, action_id)): Path<(String, String)>,
    Json(body): Json<ActionBody>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    check_csrf(&state, &headers, &s)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("agent"));
    }
    let payload = serde_json::to_value(&body).map_err(anyhow::Error::from)?;
    let cmd = crate::idempotency::Command::prepare(
        &s.actor.id,
        "POST",
        format!("/agent/sessions/{id}/actions/{action_id}/reject"),
        &headers,
        payload,
    );
    if let Some(hit) = cmd.replay(&state.pool).await? {
        return Ok(replay_response(hit));
    }
    let value = crate::ai::reject_action(&state.pool, &s.actor, &id, &action_id, body.expected_revision).await?;
    cmd.remember(&state.pool, 200, &value).await?;
    Ok((StatusCode::OK, Json(value)).into_response())
}

async fn sse_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> AppResult<Response> {
    let s = require_user(&state, &headers).await?;
    let explicit = if let Some(raw) = headers.get("last-event-id").and_then(|value| value.to_str().ok()) {
        raw.parse::<i64>().ok()
    } else {
        q.get("after").and_then(|value| value.parse::<i64>().ok())
    };
    let feed = crate::events::feed(
        &state.pool,
        explicit,
        Some(&s.actor.id),
        s.actor.role == "owner",
        100,
    )
    .await
    .map_err(anyhow::Error::from)?;
    if event_snapshot(&headers, &q) {
        return Ok(Json(serde_json::json!({
            "event_cursor": feed.cursor,
            "stale": feed.stale,
            "events": feed.events
        }))
        .into_response());
    }
    let cursor = explicit.unwrap_or(feed.cursor);
    let pending: VecDeque<serde_json::Value> = if feed.stale {
        VecDeque::new()
    } else {
        feed.events.into()
    };
    let pool = state.pool.clone();
    let session_id = s.id.clone();
    let actor_id = s.actor.id.clone();
    let owner = s.actor.role == "owner";
    let stale_cursor = feed.stale.then_some(feed.cursor);
    let stream = stream::unfold(
        (pool, session_id, actor_id, owner, cursor, pending, stale_cursor, false),
        |(pool, session_id, actor_id, owner, mut cursor, mut pending, mut stale_cursor, mut closed)| async move {
            if closed {
                return None;
            }
            if !crate::auth::session_open(&pool, &session_id).await.unwrap_or(false) {
                return None;
            }
            if let Some(cursor) = stale_cursor.take() {
                closed = true;
                return Some((
                    Ok::<_, Infallible>(sse_resync(cursor)),
                    (pool, session_id, actor_id, owner, cursor, pending, stale_cursor, closed),
                ));
            }
            if let Some(event) = pending.pop_front() {
                if let Some(id) = event["id"].as_i64() {
                    cursor = id;
                }
                let sse = sse_data(&event);
                return Some((
                    Ok::<_, Infallible>(sse),
                    (pool, session_id, actor_id, owner, cursor, pending, stale_cursor, closed),
                ));
            }
            loop {
                tokio::time::sleep(Duration::from_millis(150)).await;
                if !crate::auth::session_open(&pool, &session_id).await.unwrap_or(false) {
                    return None;
                }
                let floor = match crate::events::event_floor(&pool).await {
                    Ok(floor) => floor,
                    Err(_) => continue,
                };
                if cursor < floor {
                    closed = true;
                    let watermark = crate::events::feed(&pool, None, Some(&actor_id), owner, 1)
                        .await
                        .map(|feed| feed.cursor)
                        .unwrap_or(floor);
                    return Some((
                        Ok::<_, Infallible>(sse_resync(watermark)),
                        (pool, session_id, actor_id, owner, cursor, pending, stale_cursor, closed),
                    ));
                }
                let more = match crate::events::after(&pool, cursor, Some(&actor_id), owner, 100).await {
                    Ok(more) => more,
                    Err(_) => continue,
                };
                let Some(first) = more.into_iter().next() else {
                    continue;
                };
                let rest: Vec<_> = crate::events::after(&pool, first["id"].as_i64().unwrap_or(cursor), Some(&actor_id), owner, 100)
                    .await
                    .unwrap_or_default();
                if let Some(id) = first["id"].as_i64() {
                    cursor = id;
                }
                pending.extend(rest);
                return Some((
                    Ok::<_, Infallible>(sse_data(&first)),
                    (pool, session_id, actor_id, owner, cursor, pending, stale_cursor, closed),
                ));
            }
        },
    );
    let mut response = Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-cache, no-transform".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    Ok(response)
}

fn event_snapshot(headers: &HeaderMap, query: &std::collections::HashMap<String, String>) -> bool {
    if query.get("snapshot").map(String::as_str) == Some("1") {
        return true;
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    accept.contains("application/json") && !accept.contains("text/event-stream")
}

fn sse_data(event: &serde_json::Value) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .id(event["id"].as_i64().unwrap_or(0).to_string())
        .event(event["type"].as_str().unwrap_or("message"))
        .json_data(event)
        .unwrap()
}

fn sse_resync(cursor: i64) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("resync")
        .id(cursor.to_string())
        .json_data(serde_json::json!({"type": "resync", "event_cursor": cursor}))
        .unwrap()
}

fn admin_aliases() -> &'static [&'static str] {
    &[
        "/admin/",
        "/administrator",
        "/administrator/",
        "/manage",
        "/manage/",
        "/management",
        "/management/",
        "/manager",
        "/manager/",
        "/dashboard",
        "/dashboard/",
        "/console",
        "/console/",
        "/panel",
        "/panel/",
        "/admin-panel",
        "/admin-panel/",
        "/backend",
        "/backend/",
        "/admin.php",
        "/admin.html",
        "/admin/index.php",
        "/admin/index.html",
        "/admin/login",
        "/admin/login/",
        "/backend/login",
        "/login.php",
        "/wp-admin",
        "/wp-admin/",
        "/wp-login.php",
    ]
}

async fn admin_or_spa(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    if matches!(method, Method::GET | Method::HEAD) {
        if is_admin_alias(&path) {
            return page_redirect(StatusCode::PERMANENT_REDIRECT, "/admin", false);
        }
        let next = safe_next(query_param(req.uri().query(), "next"));
        if path.eq_ignore_ascii_case("/admin") && !path.contains('%') {
            let initialized = auth::initialized(&state.pool).await.unwrap_or(false);
            let session = session_from(&state, req.headers()).await;
            let location = if !initialized {
                "/admin/setup".to_string()
            } else if session.as_ref().map(|s| s.actor.role.as_str()) == Some("owner") {
                next.unwrap_or_else(|| "/settings/general".to_string())
            } else if session.is_none() {
                match next {
                    Some(target) => format!("/login?next={}", encode_query_component(&target)),
                    None => "/login".to_string(),
                }
            } else {
                "/admin/unavailable".to_string()
            };
            return page_redirect(StatusCode::SEE_OTHER, &location, true);
        }
        if path.eq_ignore_ascii_case("/login") && !path.contains('%') {
            let initialized = auth::initialized(&state.pool).await.unwrap_or(false);
            if !initialized {
                return page_redirect(StatusCode::SEE_OTHER, "/admin/setup", true);
            }
            if let Some(session) = session_from(&state, req.headers()).await {
                let location = if session.actor.role == "owner" {
                    next.unwrap_or_else(|| "/settings/general".to_string())
                } else {
                    "/admin/unavailable".to_string()
                };
                return page_redirect(StatusCode::SEE_OTHER, &location, true);
            }
        }
        if let Some(root) = &state.config.web_root {
            let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
            let accept_html = req.headers().get(header::ACCEPT).and_then(|value| value.to_str().ok())
                .is_some_and(|accept| accept.split(',').any(|item| item.trim().starts_with("text/html")));
            let app_page = matches!(first, "" | "albums" | "photos" | "library" | "uploads" | "downloads" | "tasks" | "trash" | "settings" | "agent" | "login")
                || matches!(path.as_str(), "/admin/setup" | "/admin/unavailable");
            let reserved = matches!(first, "api" | "health" | "assets")
                || path.rsplit('/').next().is_some_and(|last| last.contains('.'));
            if !reserved && (app_page || accept_html) {
                if let Ok(html) = tokio::fs::read(root.join("index.html")).await {
                    let mut response = Response::new(if method == Method::HEAD { Body::empty() } else { Body::from(html) });
                    *response.status_mut() = if app_page { StatusCode::OK } else { StatusCode::NOT_FOUND };
                    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
                    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                    response.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
                    response.headers_mut().insert("referrer-policy", HeaderValue::from_static("same-origin"));
                    return response;
                }
                return (StatusCode::SERVICE_UNAVAILABLE, "frontend unavailable").into_response();
            }
        }
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn is_admin_alias(path: &str) -> bool {
    path.is_ascii()
        && !path.contains('%')
        && admin_aliases()
            .iter()
            .any(|alias| path.eq_ignore_ascii_case(alias))
}

fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn safe_next(raw: Option<&str>) -> Option<String> {
    let decoded = percent_decode(raw?)?;
    if !decoded.starts_with('/') || decoded.starts_with("//") || decoded.starts_with("/\\") {
        return None;
    }
    if decoded.contains('\\')
        || decoded.contains("://")
        || decoded.contains('@')
        || decoded.contains('#')
        || decoded.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return None;
    }
    let path = decoded.split('?').next().unwrap_or("");
    if path.split('/').any(|segment| segment == "." || segment == "..") {
        return None;
    }
    Some(decoded)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_nibble(bytes[index + 1])?;
            let low = hex_nibble(bytes[index + 2])?;
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_query_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn page_redirect(status: StatusCode, location: &str, no_store: bool) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .unwrap();
    if no_store {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

pub async fn issue_bootstrap_token(pool: &sqlx::SqlitePool) -> anyhow::Result<String> {
    let token = random_token(8);
    let sha = sha256_hex(token.as_bytes());
    sqlx::query("INSERT INTO bootstrap_tokens(token_sha256, expires_at) VALUES(?, ?)")
        .bind(&sha)
        .bind(crate::db::now_ms() + 15 * 60 * 1000)
        .execute(pool)
        .await?;
    Ok(token)
}
