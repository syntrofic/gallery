use serde_json::{json, Value};

use crate::app::AppState;

pub async fn readiness(state: &AppState) -> (bool, Value) {
    if !state.ready.load(std::sync::atomic::Ordering::Relaxed) {
        return (
            false,
            json!({"status": "unavailable", "sqlite": "unavailable", "media": "unavailable", "maintenance": false}),
        );
    }
    let sqlite_ok = sqlx::query_as::<_, (i64,)>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
    if !sqlite_ok {
        return (
            false,
            json!({"status": "unavailable", "sqlite": "unavailable", "media": "unchecked", "maintenance": false}),
        );
    }
    let route = sqlx::query_as::<_, (i64, String)>(
        "SELECT write_barrier, active_backend_id FROM storage_route WHERE id = 1",
    )
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some((barrier, _backend_id)) = route.filter(|(_, backend_id)| !backend_id.is_empty()) else {
        return (
            false,
            json!({"status": "unavailable", "sqlite": "ok", "media": "unavailable", "maintenance": false}),
        );
    };
    let media_ok = if state.storage.kind == "local" {
        state.config.media_root().is_dir()
    } else {
        true
    };
    if !media_ok {
        return (
            false,
            json!({"status": "unavailable", "sqlite": "ok", "media": "unavailable", "maintenance": barrier == 1}),
        );
    }
    (
        true,
        json!({
            "status": "ok",
            "sqlite": "ok",
            "media": "ok",
            "maintenance": barrier == 1
        }),
    )
}
