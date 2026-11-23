use serde_json::Value;

#[test]
fn colon_actions_rewrite_to_slash_routes() {
    assert_eq!(
        gallery::scheduler::rewrite_colon_path("/api/v1/jobs/abc:cancel"),
        "/api/v1/jobs/abc/cancel"
    );
    assert_eq!(
        gallery::scheduler::rewrite_colon_path("/api/v1/albums/x/downloads:selected"),
        "/api/v1/albums/x/downloads/selected"
    );
}

#[test]
fn openapi_lists_implemented_routes() {
    let openapi = include_str!("../openapi/gallery-v1.yaml");
    let routes = [
        "/setup/status",
        "/setup/owner",
        "/session",
        "/admin/account/password",
        "/account/password",
        "/admin/users/{user_id}/reset-password",
        "/admin/storage/probe",
        "/admin/backups/{id}",
        "/admin/restores",
        "/admin/jobs/summary",
        "/admin/audit.jsonl",
        "/admin/variants/stats",
        "/admin/search/rebuild",
        "/admin/job-pauses/{kind}",
        "/admin/storage-migrations/{id}/start",
        "/admin/storage-migrations/{id}/pause",
        "/admin/storage-migrations/{id}/resume",
        "/admin/storage-migrations/{id}/abort",
        "/admin/storage-migrations/{id}/rollback",
        "/admin/storage-migrations/{id}/items",
        "/admin/storage-migrations/{id}/cutover",
        "/admin/storage-migrations/{id}/retire",
        "/albums/{album_id}/images/{image_id}/original-view",
        "/albums/{album_id}/images/{image_id}/original-download",
        "/albums/{album_id}/items/add",
        "/albums/{album_id}/items/rename",
        "/albums/{album_id}/downloads/selected",
        "/uploads/{batch_id}/items/{item_id}/body",
        "/downloads/claims/{claim_id}/body",
        "/archives/{id}/body",
        "/jobs/{id}/cancel",
        "/jobs/{id}/retry",
        "/variant-profiles/current",
        "/conversions/{id}/outputs/{output_id}/body",
        "/images/{image_id}/trash",
        "/images/{image_id}/purge",
        "/library/images/{image_id}",
        "/library/images/{image_id}/original-view",
        "/library/images/{image_id}/original-download",
        "/library/images/{image_id}/variants/{variant_id}/body",
        "/ai/scans/{id}",
        "/ai/scans/{id}/cancel",
        "/agent/sessions/{id}/messages/{message_id}/remove",
        "/agent/sessions/{id}/messages/{message_id}/resend",
        "/agent/sessions/{id}/turns/{turn_id}/stop",
        "/events",
    ];
    for route in routes {
        assert!(openapi.contains(route), "missing {route}");
    }
}

#[test]
fn openapi_and_examples_share_job_shape() {
    let openapi = include_str!("../openapi/gallery-v1.yaml");
    assert!(openapi.contains("/session"));
    assert!(openapi.contains("Problem"));
    let job: Value = serde_json::from_str(include_str!("../contracts/examples/job.json")).unwrap();
    for key in ["id", "kind", "state", "status_url", "progress"] {
        assert!(job.get(key).is_some(), "{key}");
    }
    assert_eq!(job["progress"]["done"], 0);
}

#[test]
fn openapi_covers_every_router_path() {
    let routing = include_str!("../src/scheduler.rs").split("pub fn router(").nth(1).unwrap()
        .split("async fn record_request_faults").next().unwrap();
    let openapi = include_str!("../openapi/gallery-v1.yaml");
    for route in routing.split(".route(").skip(1) {
        let path = route.trim_start().split('"').nth(1).unwrap();
        let paths = openapi.lines().filter_map(|line| line.strip_prefix("  "))
            .filter_map(|line| line.strip_suffix(':'))
            .map(|key| key.trim_matches(['"', '\'']));
        assert!(paths.into_iter().any(|key| key == path), "missing API path {path}");
    }
}
