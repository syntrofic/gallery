use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use gallery::app::bootstrap;
use gallery::config::Config;
use gallery::scheduler::router;
use http_body_util::BodyExt;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    cookie: String,
    csrf: String,
}

impl Harness {
    async fn new(dir: PathBuf) -> Self {
        let mut cfg = Config::from_env().unwrap();
        cfg.data_dir = dir;
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        cfg.public_url = "http://127.0.0.1:8080".into();
        cfg.admin_username = "admin".into();
        cfg.admin_password = Some("owner-pass-12".into());
        cfg.ai_enabled = false;
        let state = bootstrap(cfg, true).await.unwrap();
        Self {
            app: router(state).layer(axum::middleware::from_fn(gallery::http_util::request_id_mw)),
            cookie: String::new(),
            csrf: String::new(),
        }
    }

    async fn call(&self, mut req: Request<Body>) -> (StatusCode, header::HeaderMap, Vec<u8>) {
        if !self.cookie.is_empty() {
            req.headers_mut().insert(
                header::COOKIE,
                format!("gallery_session={}", self.cookie).parse().unwrap(),
            );
        }
        if !self.csrf.is_empty()
            && !matches!(
                req.method().as_str(),
                "GET" | "HEAD" | "OPTIONS"
            )
        {
            req.headers_mut()
                .insert("x-csrf-token", self.csrf.parse().unwrap());
            req.headers_mut()
                .insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        }
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, headers, body)
    }

    async fn login(&mut self) {
        let (s, headers, body) = self
            .call(
                Request::post("/api/v1/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"owner-pass-12"}"#))
                    .unwrap(),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        let set_cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
        self.cookie = set_cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("gallery_session=")
            .unwrap()
            .to_string();
        self.csrf = serde_json::from_slice::<Value>(&body).unwrap()["csrf_token"]
            .as_str()
            .unwrap()
            .to_string();
    }

    async fn json(&self, req: Request<Body>) -> (StatusCode, Value) {
        let (s, _, b) = self.call(req).await;
        let v = serde_json::from_slice(&b).unwrap_or(Value::Null);
        (s, v)
    }
}

fn png_bytes(w: u32, h: u32, transparent: bool) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let a = if transparent && x < 4 { 0 } else { 255 };
        *p = image::Rgba([x as u8, y as u8, 90, a]);
    }
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

#[tokio::test]
async fn contract_examples_exist() {
    let job: Value = serde_json::from_str(include_str!("../contracts/examples/job.json")).unwrap();
    assert_eq!(job["kind"], "generate_variant");
    let problem: Value =
        serde_json::from_str(include_str!("../contracts/examples/problem.json")).unwrap();
    assert_eq!(problem["code"], "NOT_FOUND");
    let session: Value =
        serde_json::from_str(include_str!("../contracts/examples/session-anonymous.json")).unwrap();
    assert!(session["actor"].is_null());
}

#[tokio::test]
async fn local_gallery_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;

    let (s, v) = h
        .json(Request::get("/api/v1/setup/status").body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["initialized"], true);

    let (s, headers, body) = h
        .call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"admin","password":"owner-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let set_cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    h.cookie = set_cookie
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("gallery_session=")
        .unwrap()
        .to_string();
    let session: Value = serde_json::from_slice(&body).unwrap();
    h.csrf = session["csrf_token"].as_str().unwrap().to_string();
    assert_eq!(session["actor"]["role"], "owner");

    let (s, _) = h
        .json(Request::get("/health/live").body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::OK);

    let (s, headers, _) = h
        .call(Request::get("/wp-admin").body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/admin");

    let (s, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Coast","description":"test"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    assert!(album["capabilities"]["upload"].as_bool().unwrap());

    let png = png_bytes(64, 48, true);
    let (s, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap().to_string();
    let batch_id = batch["id"].as_str().unwrap().to_string();

    let (s, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();

    let mut published = false;
    let mut last_list = Value::Null;
    let mut last_jobs = Value::Null;
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, listed) = h
            .json(
                Request::get(format!("/api/v1/albums/{album_id}/images"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        last_list = listed.clone();
        let (_, jobs) = h
            .json(Request::get("/api/v1/jobs").body(Body::empty()).unwrap())
            .await;
        last_jobs = jobs;
        if listed["items"][0]["variants"]
            .as_array()
            .map(|a| a.iter().any(|v| v["spec_id"] == "thumb"))
            .unwrap_or(false)
        {
            published = true;
            break;
        }
    }
    assert!(
        published,
        "thumb variant should publish; images={last_list} jobs={last_jobs}"
    );

    let (_, detail) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(detail.get("original_url").is_none() || detail["original_url"].is_null());
    let variants = detail["variants"].as_array().cloned().unwrap_or_default();
    let thumb = variants
        .iter()
        .find(|v| v["spec_id"] == "thumb")
        .expect("thumb");
    let vid = thumb["id"].as_str().unwrap();
    let (s, _, body) = h
        .call(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/variants/{vid}/body"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.len() > 32);

    let (s, _, _) = h
        .call(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/original-view"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    let (_, current) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let (s, patch) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "cover_image_id": image_id,
                        "expected_revision": current["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(patch["cover_image_id"], image_id);

    let (s, sel) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/downloads/selected"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"image_ids":[image_id]}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "selected download: {sel}");
    let claim = sel["items"][0]["claim_id"].as_str().unwrap();
    let (s, _, orig) = h
        .call(
            Request::get(format!("/api/v1/downloads/claims/{claim}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(orig, png);

    let (s, arch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/archives"))
                .header("x-gallery-client", "desktop")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(s == StatusCode::ACCEPTED || s == StatusCode::OK, "{s} {arch}");
    let archive_id = arch["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, st) = h
            .json(
                Request::get(format!("/api/v1/archives/{archive_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        if st["state"] == "ready" {
            ready = true;
            break;
        }
    }
    assert!(ready, "archive should become ready");
    let (s, _, zip_bytes) = h
        .call(
            Request::get(format!("/api/v1/archives/{archive_id}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(zip_bytes.len() > png.len());

    let (s, jobs) = h
        .json(Request::get("/api/v1/jobs").body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(jobs["items"].as_array().unwrap().iter().any(|j| j["kind"] == "generate_variant"));

    let (s, scan) = h
        .json(
            Request::post("/api/v1/ai/scans")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"scope":{}}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(scan["code"], "FEATURE_GATED");

    let (_, listed) = h
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(listed["items"][0]["id"], album_id);

    let (s, _) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/trash"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_lifecycle_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn anonymous_session_and_problem_json() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path().to_path_buf()).await;
    let (s, v) = h
        .json(Request::get("/api/v1/session").body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["actor"].is_null());
    let (s, headers, body) = h
        .call(Request::get("/api/v1/albums/nope").body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/problem+json"
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], "NOT_FOUND");
    assert!(v["request_id"].as_str().unwrap().len() > 4);
}

#[tokio::test]
async fn local_storage_probe() {
    let dir = tempfile::tempdir().unwrap();
    let backend = gallery::storage::local::LocalFs::new(dir.path().join("media"));
    let caps = gallery::storage::MediaBackend::probe(&backend).await.unwrap();
    assert!(caps.range);
    let key = "media/ab/img/file.png";
    gallery::storage::MediaBackend::put_immutable(&backend, key, b"hello", "image/png")
        .await
        .unwrap();
    let got = gallery::storage::MediaBackend::open(&backend, key, None)
        .await
        .unwrap();
    assert_eq!(got, b"hello");
}

#[tokio::test]
async fn s3_put_file_streams_a_multipart_archive() {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    static OBJECTS: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    static UPLOADS: OnceLock<Mutex<HashMap<String, BTreeMap<i32, Vec<u8>>>>> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(1);
    OBJECTS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    UPLOADS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let objects = OBJECTS.get_or_init(|| Mutex::new(HashMap::new()));
        let uploads = UPLOADS.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let param = |name: &str| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (key == name).then(|| value.to_string())
            })
        };
        let has = |name: &str| query.split('&').any(|pair| pair.split('=').next() == Some(name));
        if *req.method() == axum::http::Method::POST && has("uploads") && !query.contains("uploadId") {
            let id = format!("upload-{}", NEXT.fetch_add(1, Ordering::Relaxed));
            uploads.lock().unwrap().insert(id.clone(), BTreeMap::new());
            let xml = format!(r#"<?xml version="1.0" encoding="UTF-8"?><InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"#);
            return (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], xml).into_response();
        }
        if let Some(upload_id) = param("uploadId") {
            if *req.method() == axum::http::Method::PUT {
                let number: i32 = param("partNumber").unwrap_or_default().parse().unwrap_or(1);
                let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                uploads.lock().unwrap().entry(upload_id).or_default().insert(number, bytes);
                return axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header(header::ETAG, format!("\"part-{number}\""))
                    .body(Body::empty())
                    .unwrap();
            }
            if *req.method() == axum::http::Method::POST {
                let parts = uploads.lock().unwrap().remove(&upload_id).unwrap_or_default();
                let mut body = Vec::new();
                for (_, chunk) in parts {
                    body.extend(chunk);
                }
                objects.lock().unwrap().insert(path, body);
                let xml = r#"<?xml version="1.0" encoding="UTF-8"?><CompleteMultipartUploadResult><ETag>"done"</ETag></CompleteMultipartUploadResult>"#;
                return (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], xml).into_response();
            }
        }
        if *req.method() == axum::http::Method::GET {
            if let Some(bytes) = objects.lock().unwrap().get(&path).cloned() {
                let start = req
                    .headers()
                    .get(header::RANGE)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("bytes="))
                    .and_then(|value| value.split_once('-'))
                    .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)));
                if let Some((start, end)) = start {
                    return (StatusCode::PARTIAL_CONTENT, bytes[start..=end].to_vec()).into_response();
                }
                return (StatusCode::OK, bytes).into_response();
            }
            return StatusCode::NOT_FOUND.into_response();
        }
        if *req.method() == axum::http::Method::HEAD {
            return if objects.lock().unwrap().contains_key(&path) {
                StatusCode::OK.into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            };
        }
        StatusCode::OK.into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let payload = b"zip-bytes-streamed";
    let file = dir.path().join("pack.zip");
    std::fs::write(&file, payload).unwrap();
    let backend = gallery::storage::s3::S3Backend::new(
        Some(format!("http://{addr}")),
        "us-east-1".into(),
        "gallery".into(),
        "app".into(),
        Some("test-key".into()),
        Some("test-secret".into()),
    )
    .with_multipart_threshold(1, 4);
    let meta = gallery::storage::MediaBackend::put_file(&backend, "archives/pack.zip", &file, "application/zip")
        .await
        .unwrap();
    assert_eq!(meta.size, payload.len() as u64);
    assert_eq!(meta.sha256, gallery::ids::sha256_hex(payload));
    let got = gallery::storage::MediaBackend::open(&backend, "archives/pack.zip", None)
        .await
        .unwrap();
    assert_eq!(got, payload);
    let parts = UPLOADS.get().unwrap().lock().unwrap().len();
    assert_eq!(parts, 0);
}

#[tokio::test]
async fn s3_adapter_roundtrip() {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static STORE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
    store.lock().unwrap().clear();
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().to_string();
        match *req.method() {
            axum::http::Method::PUT => {
                let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                let mut map = store.lock().unwrap();
                if let Some(existing) = map.get(&path) {
                    if existing == &bytes {
                        return StatusCode::OK.into_response();
                    }
                    return StatusCode::PRECONDITION_FAILED.into_response();
                }
                map.insert(path, bytes);
                StatusCode::OK.into_response()
            }
            axum::http::Method::GET => {
                if let Some(bytes) = store.lock().unwrap().get(&path).cloned() {
                    (StatusCode::OK, bytes).into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            axum::http::Method::HEAD => {
                if let Some(bytes) = store.lock().unwrap().get(&path).cloned() {
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_LENGTH, bytes.len())
                        .body(Body::empty())
                        .unwrap()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            axum::http::Method::DELETE => {
                store.lock().unwrap().remove(&path);
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::OK.into_response(),
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let backend = gallery::storage::s3::S3Backend::new(
        Some(format!("http://{addr}")),
        "us-east-1".into(),
        "gallery".into(),
        "app".into(),
        Some("test-key".into()),
        Some("test-secret".into()),
    );
    gallery::storage::MediaBackend::put_immutable(&backend, "a/b.bin", b"s3-bytes", "application/octet-stream")
        .await
        .unwrap();
    gallery::storage::MediaBackend::put_immutable(&backend, "a/b.bin", b"s3-bytes", "application/octet-stream")
        .await
        .unwrap();
    let got = gallery::storage::MediaBackend::open(&backend, "a/b.bin", None)
        .await
        .unwrap();
    assert_eq!(got, b"s3-bytes");
    gallery::storage::MediaBackend::delete(&backend, "a/b.bin")
        .await
        .unwrap();
    let missing = gallery::storage::MediaBackend::head(&backend, "a/b.bin")
        .await
        .unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn s3_lifecycle_aborts_incomplete_multipart_uploads() {
    use std::sync::{Mutex, OnceLock};
    static RULE: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
    RULE.get_or_init(|| Mutex::new(Vec::new())).lock().unwrap().clear();
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let rule = RULE.get_or_init(|| Mutex::new(Vec::new()));
        let query = req.uri().query().unwrap_or("");
        if !query.split('&').any(|pair| pair.split('=').next() == Some("lifecycle")) {
            return StatusCode::NOT_FOUND.into_response();
        }
        if *req.method() == axum::http::Method::PUT {
            let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
            *rule.lock().unwrap() = bytes;
            return StatusCode::OK.into_response();
        }
        if *req.method() == axum::http::Method::GET {
            let bytes = rule.lock().unwrap().clone();
            if bytes.is_empty() {
                return StatusCode::NOT_FOUND.into_response();
            }
            return (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], bytes).into_response();
        }
        StatusCode::METHOD_NOT_ALLOWED.into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let backend = gallery::storage::s3::S3Backend::new(
        Some(format!("http://{addr}")),
        "us-east-1".into(),
        "gallery".into(),
        "app".into(),
        Some("test-key".into()),
        Some("test-secret".into()),
    );
    let days = backend.ensure_multipart_lifecycle().await.unwrap();
    assert_eq!(days, 1);
    let stored = RULE.get().unwrap().lock().unwrap().clone();
    let xml = String::from_utf8(stored).unwrap();
    assert!(xml.contains("gallery-abort-incomplete-multipart"));
    assert!(xml.contains("AbortIncompleteMultipartUpload"));
    assert!(xml.contains("app/"));
}

#[tokio::test]
async fn s3_lists_stale_uploads_and_aborts_when_the_record_fails() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    static UPLOADS: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(1);
    UPLOADS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let uploads = UPLOADS.get_or_init(|| Mutex::new(HashMap::new()));
        let query = req.uri().query().unwrap_or("").to_string();
        let has = |name: &str| query.split('&').any(|pair| pair.split('=').next() == Some(name));
        let param = |name: &str| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (key == name).then(|| value.to_string())
            })
        };
        if *req.method() == axum::http::Method::GET && has("uploads") {
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><ListMultipartUploadsResult><IsTruncated>false</IsTruncated>{}</ListMultipartUploadsResult>"#,
                uploads
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, stale)| {
                        let initiated = if *stale { "2020-01-01T00:00:00.000Z" } else { "2099-01-01T00:00:00.000Z" };
                        format!("<Upload><Key>app/{id}.bin</Key><UploadId>{id}</UploadId><Initiated>{initiated}</Initiated></Upload>")
                    })
                    .collect::<String>()
            );
            return (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], xml).into_response();
        }
        if *req.method() == axum::http::Method::POST && has("uploads") {
            let id = format!("upload-{}", NEXT.fetch_add(1, Ordering::Relaxed));
            uploads.lock().unwrap().insert(id.clone(), false);
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"#
            );
            return (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], xml).into_response();
        }
        if *req.method() == axum::http::Method::DELETE {
            if let Some(id) = param("uploadId") {
                uploads.lock().unwrap().remove(&id);
            }
            return StatusCode::NO_CONTENT.into_response();
        }
        StatusCode::OK.into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    {
        let mut uploads = UPLOADS.get().unwrap().lock().unwrap();
        uploads.insert("old-upload".into(), true);
        uploads.insert("fresh-upload".into(), false);
    }
    let backend = gallery::storage::s3::S3Backend::new(
        Some(format!("http://{addr}")),
        "us-east-1".into(),
        "gallery".into(),
        "app".into(),
        Some("test-key".into()),
        Some("test-secret".into()),
    )
    .with_multipart_threshold(1, 4);
    let aborted = gallery::storage::MediaBackend::abort_stale_multiparts(&backend, gallery::db::now_ms())
        .await
        .unwrap();
    assert_eq!(aborted, 1);
    let left: Vec<_> = UPLOADS.get().unwrap().lock().unwrap().keys().cloned().collect();
    assert_eq!(left, vec!["fresh-upload".to_string()]);
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    sqlx::query("DROP TABLE storage_multipart_uploads")
        .execute(&pool)
        .await
        .unwrap();
    gallery::storage::MediaBackend::bind_catalog(&backend, pool, "be-s3".into());
    let failed = gallery::storage::MediaBackend::put_immutable(
        &backend,
        "lost.bin",
        b"abcdef",
        "application/octet-stream",
    )
    .await;
    assert!(failed.is_err());
    assert!(UPLOADS.get().unwrap().lock().unwrap().keys().all(|id| id == "fresh-upload"));
}

#[tokio::test]
async fn s3_multipart_verifies_parts_and_aborts_when_complete_fails() {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    static OBJECTS: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    static UPLOADS: OnceLock<Mutex<HashMap<String, BTreeMap<i32, Vec<u8>>>>> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(1);
    static FAIL_COMPLETE: AtomicBool = AtomicBool::new(false);
    OBJECTS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    UPLOADS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    FAIL_COMPLETE.store(false, Ordering::Relaxed);
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let objects = OBJECTS.get_or_init(|| Mutex::new(HashMap::new()));
        let uploads = UPLOADS.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let param = |name: &str| {
            query.split('&').find_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?;
                let value = parts.next().unwrap_or("");
                (key == name).then(|| value.to_string())
            })
        };
        let has = |name: &str| query.split('&').any(|pair| pair.split('=').next() == Some(name));
        if *req.method() == axum::http::Method::POST && has("uploads") {
            let id = format!("upload-{}", NEXT.fetch_add(1, Ordering::Relaxed));
            uploads.lock().unwrap().insert(id.clone(), BTreeMap::new());
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><InitiateMultipartUploadResult><Bucket>gallery</Bucket><Key>{path}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"#
            );
            return (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], xml).into_response();
        }
        if let Some(upload_id) = param("uploadId") {
            match *req.method() {
                axum::http::Method::PUT => {
                    let number: i32 = param("partNumber").unwrap_or_default().parse().unwrap_or(1);
                    let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                    uploads.lock().unwrap().entry(upload_id).or_default().insert(number, bytes);
                    return axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header(header::ETAG, format!("\"part-{number}\""))
                        .body(Body::empty())
                        .unwrap();
                }
                axum::http::Method::POST => {
                    if FAIL_COMPLETE.load(Ordering::Relaxed) {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    let parts = uploads.lock().unwrap().remove(&upload_id).unwrap_or_default();
                    let mut body = Vec::new();
                    for (_, chunk) in parts {
                        body.extend(chunk);
                    }
                    objects.lock().unwrap().insert(path.clone(), body);
                    let xml = format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?><CompleteMultipartUploadResult><Bucket>gallery</Bucket><Key>{path}</Key><ETag>"done"</ETag></CompleteMultipartUploadResult>"#
                    );
                    return (StatusCode::OK, [(header::CONTENT_TYPE, "application/xml")], xml).into_response();
                }
                axum::http::Method::DELETE => {
                    uploads.lock().unwrap().remove(&upload_id);
                    return StatusCode::NO_CONTENT.into_response();
                }
                _ => {}
            }
        }
        match *req.method() {
            axum::http::Method::PUT => {
                let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                let mut map = objects.lock().unwrap();
                if let Some(existing) = map.get(&path) {
                    if existing == &bytes {
                        return StatusCode::OK.into_response();
                    }
                    return StatusCode::PRECONDITION_FAILED.into_response();
                }
                map.insert(path, bytes);
                StatusCode::OK.into_response()
            }
            axum::http::Method::GET => {
                if let Some(bytes) = objects.lock().unwrap().get(&path).cloned() {
                    (StatusCode::OK, bytes).into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            axum::http::Method::HEAD => {
                if let Some(bytes) = objects.lock().unwrap().get(&path).cloned() {
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_LENGTH, bytes.len())
                        .body(Body::empty())
                        .unwrap()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            axum::http::Method::DELETE => {
                objects.lock().unwrap().remove(&path);
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::OK.into_response(),
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let backend = gallery::storage::s3::S3Backend::new(
        Some(format!("http://{addr}")),
        "us-east-1".into(),
        "gallery".into(),
        "app".into(),
        Some("test-key".into()),
        Some("test-secret".into()),
    )
    .with_multipart_threshold(1, 4);
    let payload = b"0123456789";
    gallery::storage::MediaBackend::put_immutable(&backend, "a/large.bin", payload, "application/octet-stream")
        .await
        .unwrap();
    let got = gallery::storage::MediaBackend::open(&backend, "a/large.bin", None)
        .await
        .unwrap();
    assert_eq!(got, payload);
    gallery::storage::MediaBackend::put_immutable(&backend, "a/large.bin", payload, "application/octet-stream")
        .await
        .unwrap();
    let changed = gallery::storage::MediaBackend::put_immutable(&backend, "a/large.bin", b"different!", "application/octet-stream")
        .await;
    assert!(changed.is_err());
    FAIL_COMPLETE.store(true, Ordering::Relaxed);
    let aborted = gallery::storage::MediaBackend::put_immutable(&backend, "a/aborted.bin", b"abcdef", "application/octet-stream")
        .await;
    assert!(aborted.is_err());
    assert!(gallery::storage::MediaBackend::head(&backend, "a/aborted.bin").await.unwrap().is_none());
    assert!(UPLOADS.get().unwrap().lock().unwrap().is_empty());
}

#[tokio::test]
async fn s3_walk_pages_the_application_prefix() {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static STORE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
    store.lock().unwrap().clear();
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("");
        let param = |name: &str| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (key == name).then(|| percent_decode(value))
            })
        };
        if *req.method() == axum::http::Method::GET && param("list-type").as_deref() == Some("2") {
            let prefix = param("prefix").unwrap_or_default();
            let start = param("start-after").unwrap_or_default();
            let max_keys: usize = param("max-keys").and_then(|v| v.parse().ok()).unwrap_or(100);
            let mut keys: Vec<String> = store
                .lock()
                .unwrap()
                .keys()
                .filter_map(|stored| stored.strip_prefix("/gallery/").map(str::to_string))
                .filter(|key| prefix.is_empty() || key.starts_with(&prefix))
                .filter(|key| start.is_empty() || key.as_str() > start.as_str())
                .collect();
            keys.sort();
            let truncated = keys.len() > max_keys;
            keys.truncate(max_keys);
            let mut xml = String::from(
                r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>gallery</Name>"#,
            );
            xml.push_str(&format!("<KeyCount>{}</KeyCount>", keys.len()));
            xml.push_str(&format!("<IsTruncated>{}</IsTruncated>", truncated));
            for key in &keys {
                xml.push_str(&format!(
                    "<Contents><Key>{}</Key><Size>1</Size><StorageClass>STANDARD</StorageClass></Contents>",
                    key.replace('&', "&amp;").replace('<', "&lt;")
                ));
            }
            xml.push_str("</ListBucketResult>");
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/xml")],
                xml,
            )
                .into_response();
        }
        match *req.method() {
            axum::http::Method::PUT => {
                let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                store.lock().unwrap().insert(path, bytes);
                StatusCode::OK.into_response()
            }
            axum::http::Method::GET => {
                if let Some(bytes) = store.lock().unwrap().get(&path).cloned() {
                    (StatusCode::OK, bytes).into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            axum::http::Method::HEAD => {
                if store.lock().unwrap().contains_key(&path) {
                    StatusCode::OK.into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            _ => StatusCode::OK.into_response(),
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    store.lock().unwrap().insert("/gallery/other/nope.bin".into(), b"outside".to_vec());
    store.lock().unwrap().insert("/gallery/app/nested/".into(), Vec::new());
    let backend = gallery::storage::s3::S3Backend::new(
        Some(format!("http://{addr}")),
        "us-east-1".into(),
        "gallery".into(),
        "app".into(),
        Some("test-key".into()),
        Some("test-secret".into()),
    );
    for (key, bytes) in [("c.bin", b"c" as &[u8]), ("a.bin", b"a"), ("m/b.bin", b"b")] {
        gallery::storage::MediaBackend::put_immutable(&backend, key, bytes, "application/octet-stream")
            .await
            .unwrap();
    }
    let first = gallery::storage::MediaBackend::walk_owned(&backend, None, 2)
        .await
        .unwrap();
    assert_eq!(first, vec!["a.bin".to_string(), "c.bin".to_string()]);
    let rest = gallery::storage::MediaBackend::walk_owned(&backend, Some("c.bin"), 2)
        .await
        .unwrap();
    assert_eq!(rest, vec!["m/b.bin".to_string()]);
    assert!(gallery::storage::MediaBackend::head(&backend, "a.bin").await.unwrap().is_some());
    assert!(store.lock().unwrap().contains_key("/gallery/other/nope.bin"));
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

#[tokio::test]
async fn webdav_walk_pages_the_collection_and_refuses_dtd() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    static STORE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    static DTD: AtomicBool = AtomicBool::new(false);
    STORE.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    DTD.store(false, Ordering::Relaxed);
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().trim_end_matches('/').to_string();
        if req.method().as_str() == "PROPFIND" {
            if DTD.load(Ordering::Relaxed) {
                let xml = r#"<!DOCTYPE foo [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><d:multistatus xmlns:d="DAV:"></d:multistatus>"#;
                return (StatusCode::MULTI_STATUS, xml).into_response();
            }
            let prefix = format!("{path}/");
            let mut children: HashMap<String, bool> = HashMap::new();
            for key in store.lock().unwrap().keys() {
                let Some(rest) = key.strip_prefix(&prefix) else { continue };
                if rest.is_empty() {
                    continue;
                }
                if let Some((dir, _)) = rest.split_once('/') {
                    children.insert(format!("{prefix}{dir}/"), true);
                } else {
                    children.insert(format!("{prefix}{rest}"), false);
                }
            }
            let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">"#);
            xml.push_str(&format!(
                r#"<d:response><d:href>http://evil.example{path}/secret.bin</d:href><d:propstat><d:prop><d:resourcetype/></d:prop></d:propstat></d:response>"#
            ));
            let mut names: Vec<_> = children.into_iter().collect();
            names.sort();
            for (href, is_dir) in names {
                let kind = if is_dir { "<d:collection/>" } else { "" };
                xml.push_str(&format!(
                    r#"<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:resourcetype>{kind}</d:resourcetype></d:prop></d:propstat></d:response>"#
                ));
            }
            xml.push_str("</d:multistatus>");
            return (StatusCode::MULTI_STATUS, xml).into_response();
        }
        match *req.method() {
            axum::http::Method::PUT => {
                let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                store.lock().unwrap().insert(path, bytes);
                StatusCode::CREATED.into_response()
            }
            axum::http::Method::GET => {
                if let Some(bytes) = store.lock().unwrap().get(&path).cloned() {
                    (StatusCode::OK, bytes).into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            _ => {
                if req.method().as_str() == "MKCOL" {
                    StatusCode::CREATED.into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let backend = gallery::storage::webdav::WebDavBackend::new(
        format!("http://{addr}"),
        "gallery".into(),
        None,
        None,
    );
    for (key, bytes) in [("c.bin", b"c" as &[u8]), ("a.bin", b"a"), ("sub/b.bin", b"b")] {
        gallery::storage::MediaBackend::put_immutable(&backend, key, bytes, "application/octet-stream")
            .await
            .unwrap();
    }
    let first = gallery::storage::MediaBackend::walk_owned(&backend, None, 2)
        .await
        .unwrap();
    assert_eq!(first, vec!["a.bin".to_string(), "c.bin".to_string()]);
    let rest = gallery::storage::MediaBackend::walk_owned(&backend, Some("c.bin"), 2)
        .await
        .unwrap();
    assert_eq!(rest, vec!["sub/b.bin".to_string()]);
    assert!(!first.iter().any(|key| key.contains("secret")));
    DTD.store(true, Ordering::Relaxed);
    let refused = gallery::storage::MediaBackend::walk_owned(&backend, None, 10).await;
    assert!(refused.is_err());
    let still = gallery::storage::MediaBackend::open(&backend, "a.bin", None)
        .await
        .unwrap();
    assert_eq!(still, b"a");
}

#[tokio::test]
async fn webdav_adapter_roundtrip() {
    async fn handle(req: Request<Body>) -> axum::response::Response {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        static STORE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
        let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().to_string();
        match *req.method() {
            axum::http::Method::PUT => {
                let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                store.lock().unwrap().insert(path, bytes);
                StatusCode::CREATED.into_response()
            }
            axum::http::Method::GET => {
                if let Some(b) = store.lock().unwrap().get(&path).cloned() {
                    (StatusCode::OK, b).into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            axum::http::Method::HEAD => {
                if store.lock().unwrap().contains_key(&path) {
                    StatusCode::OK.into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            axum::http::Method::DELETE => {
                store.lock().unwrap().remove(&path);
                StatusCode::NO_CONTENT.into_response()
            }
            _ => {
                if req.method().as_str() == "MKCOL" {
                    StatusCode::CREATED.into_response()
                } else {
                    StatusCode::NOT_FOUND.into_response()
                }
            }
        }
    }
    use axum::response::IntoResponse;
    let app = axum::Router::new().fallback(handle);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let backend = gallery::storage::webdav::WebDavBackend::new(
        format!("http://{addr}"),
        "dav".to_string(),
        None,
        None,
    );
    gallery::storage::MediaBackend::put_immutable(&backend, "a/b.bin", b"abc", "application/octet-stream")
        .await
        .unwrap();
    let got = gallery::storage::MediaBackend::open(&backend, "a/b.bin", None)
        .await
        .unwrap();
    assert_eq!(got, b"abc");
    gallery::storage::MediaBackend::delete(&backend, "a/b.bin")
        .await
        .unwrap();
}

#[tokio::test]
async fn abort_stops_the_backup_job_and_resume_keeps_the_sealed_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('workers_draining', '1')
         ON CONFLICT(key) DO UPDATE SET value = '1'",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let now = gallery::db::now_ms();
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES('blob-pin', 'aa', 4, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-open', 'backup', 'backup:bak-open', '{}', '{}', 'running', ?, ?, ?),
                ('job-sealed', 'backup', 'backup:bak-sealed', '{}', '{}', 'failed', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, job_id, created_at, updated_at)
         VALUES('bak-open', 'running', 'job-open', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES('bak-open', 'blob-pin')")
        .execute(&state.pool)
        .await
        .unwrap();
    gallery::backup::abandon_unsealed(&state.pool, "bak-open").await.unwrap();
    let open: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-open'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(open.0, "abandoned");
    let job: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-open'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(job.0, "canceled");
    let pins: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-open'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(pins.0, 0);
    let unsealed = gallery::backup::resume_backup(&state, &actor, "bak-open").await.unwrap_err();
    assert!(unsealed.to_string().contains("BACKUP_UNSEALED"));
    sqlx::query(
        "INSERT INTO backup_runs(id, state, job_id, manifest_digest, snapshot_at, created_at, updated_at)
         VALUES('bak-sealed', 'failed', 'job-sealed', 'digest', ?, ?, ?)",
    )
    .bind(now - 1_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES('bak-sealed', 'blob-pin')")
        .execute(&state.pool)
        .await
        .unwrap();
    let resumed = gallery::backup::resume_backup(&state, &actor, "bak-sealed").await.unwrap();
    assert_eq!(resumed["id"], "bak-sealed");
    assert_eq!(resumed["state"], "queued");
    assert_eq!(resumed["snapshot_at"], now - 1_000);
    let again: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-sealed'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(again.0, "queued");
    let kept: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-sealed'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(kept.0, 1);
    let runs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_runs WHERE id = 'bak-sealed'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(runs.0, 1);
}

#[tokio::test]
async fn backup_list_separates_snapshot_copy_complete_and_ready() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE username = 'admin'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, catalog_revision, routing_epoch, verified_at, created_at, updated_at)
         VALUES('bak-snap', 'queued', NULL, NULL, NULL, NULL, ?, ?),
                ('bak-copy', 'running', 10, 1, 2, NULL, ?, ?),
                ('bak-done', 'succeeded', 20, 3, 4, NULL, ?, ?),
                ('bak-ready', 'succeeded', 30, 5, 6, ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now + 20)
    .bind(now)
    .bind(now)
    .bind(now + 30)
    .execute(&pool)
    .await
    .unwrap();
    let listed = gallery::backup::list_backups(&pool, &actor, None).await.unwrap();
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items[0]["id"], "bak-ready");
    assert_eq!(items[0]["phase"], "ready");
    assert_eq!(items[0]["format_version"], 1);
    assert_eq!(items[0]["routing_epoch"], 6);
    assert!(items[0]["schema_version"].as_i64().unwrap() > 0);
    assert_eq!(items[1]["phase"], "complete");
    assert_eq!(items[1]["completed_at"], now + 20);
    assert_eq!(items[2]["phase"], "copy");
    assert_eq!(items[3]["phase"], "snapshot");
    let one = gallery::backup::get_backup(&pool, &actor, "bak-done").await.unwrap();
    assert_eq!(one["phase"], "complete");
    assert_eq!(one["catalog_revision"], 3);
    assert!(one["verified_at"].is_null());
}

#[tokio::test]
async fn backup_preview_reports_capacity_and_freeze_without_a_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    assert_eq!(gallery::backup::estimate_freeze_ms(2_097_152, 3), 403);
    let preview = gallery::backup::preview_backup(&state, &actor, None).await.unwrap();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["backup_repository_id"], "local");
    assert_eq!(preview["images"], 0);
    assert!(preview["freeze_ms"].as_i64().unwrap() >= 200);
    assert!(preview["capacity_ok"].is_boolean());
    let foreign = gallery::backup::preview_backup(&state, &actor, Some("remote")).await.unwrap_err();
    assert!(foreign.to_string().contains("BACKUP_REPOSITORY"));
    let runs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_runs")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(runs.0, 0);
    assert!(!gallery::db::write_barrier(&state.pool).await.unwrap());
}

#[tokio::test]
async fn a_bad_signature_or_database_makes_the_backup_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let started = gallery::backup::start_backup(&state, &actor).await.unwrap();
    let backup_id = started["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let view = gallery::backup::get_backup(&state.pool, &actor, &backup_id).await.unwrap();
        if view["state"] == "succeeded" {
            ready = true;
            break;
        }
    }
    assert!(ready, "backup should finish");
    let ok = gallery::backup::verify_backup(&state, &actor, &backup_id).await.unwrap();
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["objects_verified"], ok["objects"]);
    assert!(ok["verified_at"].as_i64().unwrap() > 0);
    let root = state.config.backup_root().join(&backup_id);
    let complete = std::fs::read(root.join("COMPLETE")).unwrap();
    let sqlite = std::fs::read(root.join("gallery.sqlite.age")).unwrap();
    std::fs::write(root.join("COMPLETE"), b"bad\nbad").unwrap();
    let signed = gallery::backup::verify_backup(&state, &actor, &backup_id).await.unwrap();
    assert_eq!(signed["ok"], false);
    assert_eq!(signed["state"], "unavailable");
    assert!(signed["exceptions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["code"] == "signature"));
    assert!(root.join("COMPLETE").is_file());
    assert!(root.join("gallery.sqlite.age").is_file());
    let missing = gallery::backup::select_restore_branch(&state.pool, &backup_id, None).await;
    assert!(missing.unwrap_err().to_string().contains("BACKUP_UNAVAILABLE"));
    std::fs::write(root.join("COMPLETE"), &complete).unwrap();
    std::fs::write(root.join("gallery.sqlite.age"), b"not-age").unwrap();
    let broken = gallery::backup::verify_backup(&state, &actor, &backup_id).await.unwrap();
    assert_eq!(broken["ok"], false);
    assert!(broken["exceptions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["code"] == "decrypt" || item["code"] == "sqlite"));
    std::fs::write(root.join("gallery.sqlite.age"), &sqlite).unwrap();
    let repaired = gallery::backup::verify_backup(&state, &actor, &backup_id).await.unwrap();
    assert_eq!(repaired["ok"], true);
    assert_eq!(repaired["state"], "succeeded");
}

#[tokio::test]
async fn backup_health_reports_snapshot_age_pins_and_the_last_drill() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    let stale = gallery::backup::backup_health(&pool, now, Some(0)).await.unwrap();
    assert_eq!(stale["rpo_exceeded"], true);
    assert!(stale["alerts"].as_array().unwrap().iter().any(|item| item == "snapshot older than 24h"));
    assert!(stale["alerts"].as_array().unwrap().iter().any(|item| item == "backup capacity"));
    let hour = 3600 * 1000;
    let day = 24 * hour;
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-old', 'succeeded', ?, ?, ?),
                ('bak-fresh', 'succeeded', ?, ?, ?),
                ('bak-bad', 'failed', NULL, ?, ?)",
    )
    .bind(now - 8 * day)
    .bind(now)
    .bind(now)
    .bind(now - hour)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES('blob-pin', 'aa', 4, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES('bak-old', 'blob-pin')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO restore_runs(id, backup_id, state, created_at, updated_at)
         VALUES('restore-drill', 'bak-fresh', 'succeeded', ?, ?)",
    )
    .bind(now - hour)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let health = gallery::backup::backup_health(&pool, now, Some(10)).await.unwrap();
    assert_eq!(health["snapshot_id"], "bak-fresh");
    assert_eq!(health["rpo_exceeded"], false);
    assert_eq!(health["failures"], 1);
    assert_eq!(health["pins_held"], 1);
    assert_eq!(health["last_drill_at"], now - hour);
    let alerts = health["alerts"].as_array().unwrap();
    assert!(alerts.iter().any(|item| item == "backup failures"));
    assert!(alerts.iter().any(|item| item == "backup pins held"));
    assert!(!alerts.iter().any(|item| item == "snapshot older than 24h"));
    assert!(!alerts.iter().any(|item| item == "restore drill overdue"));
    assert!(!gallery::backup::backup_capacity_ok(Some(4), 8));
    assert!(gallery::backup::backup_capacity_ok(None, 8));
}

#[tokio::test]
async fn promote_opens_the_isolated_copy_only_after_the_report_matches() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let now = gallery::db::now_ms();
    let epoch: (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-promote', 'succeeded', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO restore_runs(id, backup_id, state, created_at, updated_at)
         VALUES('restore-promote', 'bak-promote', 'succeeded', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let isolated_dir = state.config.work_root().join("restore").join("restore-promote");
    std::fs::create_dir_all(isolated_dir.join("media")).unwrap();
    let isolated = gallery::db::connect(&isolated_dir.join("gallery.sqlite")).await.unwrap();
    gallery::storage::bootstrap_local(&isolated, isolated_dir.join("media"))
        .await
        .unwrap();
    sqlx::query("UPDATE storage_route SET catalog_revision = 4, write_barrier = 1 WHERE id = 1")
        .execute(&isolated)
        .await
        .unwrap();
    let report = isolated_dir.join("report.json");
    std::fs::write(
        &report,
        r#"{"integrity":"ok","catalog_ok":false,"originals_missing":[]}"#,
    )
    .unwrap();
    let unclear = gallery::backup::promote_restore(&state, &actor, "restore-promote", 4, "source-ack")
        .await
        .unwrap_err();
    assert!(unclear.to_string().contains("PROMOTE_REPORT"));
    std::fs::write(
        &report,
        r#"{"integrity":"ok","catalog_ok":true,"originals_missing":[]}"#,
    )
    .unwrap();
    let stale = gallery::backup::promote_restore(&state, &actor, "restore-promote", 3, "source-ack")
        .await
        .unwrap_err();
    assert!(stale.to_string().contains("REVISION_CONFLICT"));
    let unacked = gallery::backup::promote_restore(&state, &actor, "restore-promote", 4, "  ")
        .await
        .unwrap_err();
    assert!(unacked.to_string().contains("HANDOFF_SOURCE"));
    let opened = gallery::backup::promote_restore(&state, &actor, "restore-promote", 4, "source-ack")
        .await
        .unwrap();
    assert_eq!(opened["writes"], "open");
    assert_eq!(opened["catalog_revision"], 4);
    let barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&isolated)
        .await
        .unwrap();
    assert_eq!(barrier.0, 0);
    let live: (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(live.0, epoch.0);
}

#[tokio::test]
async fn a_removed_original_stays_readable_until_its_backup_pin_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let bytes = b"held-for-backup";
    let sha = gallery::ids::sha256_hex(bytes);
    state.storage.put_bytes("media/held.bin", bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &state.storage.backend_id,
        "media/held.bin",
    )
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-held', ?, ?, 'held.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, created_at, updated_at) VALUES('bak-held', 'running', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("DELETE FROM images WHERE id = 'img-held'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let dest = state.config.backup_root().join("bak-held");
    std::fs::create_dir_all(&dest).unwrap();
    gallery::backup::copy_backup_object(&state, "bak-held", &dest, &blob_id, bytes.len() as i64, &sha)
        .await
        .unwrap();
    let copied = std::fs::read(dest.join(gallery::ids::sha256_hex(blob_id.as_bytes()))).unwrap();
    let plain = gallery::backup::age_decrypt_bytes(&state.config.keys_root(), &copied).unwrap();
    assert_eq!(plain, bytes);
    let pins: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-held'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(pins.0, 1);
    sqlx::query("DELETE FROM backup_pins WHERE backup_id = 'bak-held'")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM backup_items WHERE backup_id = 'bak-held'")
        .execute(&state.pool)
        .await
        .unwrap();
    std::fs::remove_file(dest.join(gallery::ids::sha256_hex(blob_id.as_bytes()))).unwrap();
    std::fs::remove_file(dir.path().join("media").join("media").join("held.bin")).unwrap();
    assert!(gallery::backup::copy_backup_object(
        &state,
        "bak-held",
        &dest,
        &blob_id,
        bytes.len() as i64,
        &sha
    )
    .await
    .is_err());
}

#[tokio::test]
async fn early_snapshot_shows_the_gap_and_keeps_the_old_endpoint_readable() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.public_url = "http://127.0.0.1:8080".into();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let t0 = 1_700_000_000_000_i64;
    let early = b"early-original";
    let kept = b"kept-on-old-endpoint";
    let early_sha = gallery::ids::sha256_hex(early);
    let kept_sha = gallery::ids::sha256_hex(kept);
    let lost_sha = gallery::ids::sha256_hex(b"lost-on-new-endpoint");
    state
        .storage
        .put_bytes("media/early.bin", early, "application/octet-stream")
        .await
        .unwrap();
    state
        .storage
        .put_bytes("media/kept.bin", kept, "application/octet-stream")
        .await
        .unwrap();
    let source_id: (String,) = sqlx::query_as("SELECT active_backend_id FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let early_blob = gallery::storage::record_blob(
        &mut tx,
        &early_sha,
        early.len() as i64,
        "application/octet-stream",
        "original",
        &source_id.0,
        "media/early.bin",
    )
    .await
    .unwrap();
    let kept_blob = gallery::storage::record_blob(
        &mut tx,
        &kept_sha,
        kept.len() as i64,
        "application/octet-stream",
        "original",
        &source_id.0,
        "media/kept.bin",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES('blob-lost', ?, ?, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(&lost_sha)
    .bind(22_i64)
    .bind(t0)
    .execute(&mut *tx)
    .await
    .unwrap();
    let target_root = dir.path().join("missing-new-endpoint");
    let target_cfg = serde_json::json!({"kind": "local", "root": target_root}).to_string();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('be-new', 'local', ?, 1, ?)",
    )
    .bind(&target_cfg)
    .bind(t0)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, backend_revision, size, sha256, state, verified_at)
         VALUES('blob-lost', 'be-new', 'media/lost.bin', 1, ?, ?, 'verified', ?)",
    )
    .bind(22_i64)
    .bind(&lost_sha)
    .bind(t0)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES
         ('img-early', ?, ?, 'early.bin', 8, 8, 'application/octet-stream', 1, ?),
         ('img-kept', ?, ?, 'kept.bin', 8, 8, 'application/octet-stream', 2, ?),
         ('img-lost', ?, 'blob-lost', 'lost.bin', 8, 8, 'application/octet-stream', 3, ?)",
    )
    .bind(&early_sha)
    .bind(&early_blob)
    .bind(t0)
    .bind(&kept_sha)
    .bind(&kept_blob)
    .bind(t0 + 4_000)
    .bind(&lost_sha)
    .bind(t0 + 3_000)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, catalog_revision, routing_epoch, created_at, updated_at)
         VALUES('bak-early', 'succeeded', ?, 1, 1, ?, ?)",
    )
    .bind(t0 + 1_000)
    .bind(t0)
    .bind(t0)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, cutover_at, created_at, updated_at
         ) VALUES('mig-early', ?, 'be-new', 'forward', 'protected', 'protected', ?, ?, ?)",
    )
    .bind(&source_id.0)
    .bind(t0 + 2_000)
    .bind(t0)
    .bind(t0)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE storage_route SET active_backend_id = 'be-new', routing_epoch = 2 WHERE id = 1",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let backup_dir = state.config.backup_root().join("bak-early");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let manifest = br#"{"id":"bak-early"}"#;
    std::fs::write(backup_dir.join("manifest.json"), manifest).unwrap();
    std::fs::write(backup_dir.join("gallery.sqlite.age"), b"sealed-snapshot").unwrap();
    let collaborator = gallery::auth::Actor {
        id: "user-collab".into(),
        username: "collab".into(),
        role: "collaborator".into(),
        must_change_password: false,
        revision: 1,
    };
    assert!(gallery::backup::assess_early_snapshot(
        &state.pool,
        &collaborator,
        &state.config.backup_root(),
        &state.config.work_root().join("salvage"),
    )
    .await
    .is_err());
    let db_path = state.config.db_path();
    let app = gallery::scheduler::router(state)
        .layer(axum::middleware::from_fn(gallery::http_util::request_id_mw));
    let mut h = Harness {
        app,
        cookie: String::new(),
        csrf: String::new(),
    };
    h.login().await;
    let (status, body) = h
        .json(
            Request::post("/api/v1/admin/recovery/early-snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["situation"], "pre_cutover_only");
    assert_eq!(body["confirm_required"], true);
    assert_eq!(body["restore_started"], false);
    assert_eq!(body["restore_point"]["backup_id"], "bak-early");
    assert_eq!(body["restore_point"]["snapshot_at"], t0 + 1_000);
    assert_eq!(body["cutover_at"], t0 + 2_000);
    assert_eq!(body["unbacked_images"], 2);
    assert_eq!(body["current_readable"], false);
    assert_eq!(body["source_checkable"], true);
    assert_eq!(body["source_sample"]["sha256"], early_sha);
    assert_eq!(body["backup_checkable"], true);
    assert_eq!(
        body["backup_manifest_sha"],
        gallery::ids::sha256_hex(manifest)
    );
    assert_eq!(
        body["loss_sha"],
        gallery::ids::sha256_hex(b"img-kept\nimg-lost")
    );
    assert_eq!(body["salvage_copied"][0]["id"], "img-kept");
    assert_eq!(body["salvage_copied"][0]["from"], "source");
    assert_eq!(body["salvage_blocked"][0]["id"], "img-lost");
    assert_eq!(body["salvage_blocked"][0]["reason"], "endpoint unreadable");
    let saved = std::fs::read(dir.path().join("work/salvage/bak-early").join(&kept_blob)).unwrap();
    assert_eq!(saved, kept);
    assert_eq!(std::fs::read(dir.path().join("media/media/early.bin")).unwrap(), early);
    assert_eq!(std::fs::read(backup_dir.join("manifest.json")).unwrap(), manifest);
    assert!(!target_root.exists());
    let pool = gallery::db::connect(&db_path).await.unwrap();
    let restores: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM restore_runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(restores.0, 0);
    let route: (String, i64) =
        sqlx::query_as("SELECT active_backend_id, routing_epoch FROM storage_route WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(route.0, "be-new");
    assert_eq!(route.1, 2);
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, catalog_revision, routing_epoch, created_at, updated_at)
         VALUES('bak-later', 'succeeded', ?, 2, 2, ?, ?)",
    )
    .bind(t0 + 2_500)
    .bind(t0)
    .bind(t0)
    .execute(&pool)
    .await
    .unwrap();
    let (status, later) = h
        .json(
            Request::post("/api/v1/admin/recovery/early-snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{later}");
    assert_eq!(later["situation"], "post_cutover_backup");
    assert_eq!(later["restore_point"]["backup_id"], "bak-later");
    assert_eq!(later["restore_started"], false);
    assert_eq!(std::fs::read(dir.path().join("work/salvage/bak-early").join(&kept_blob)).unwrap(), kept);
    assert_eq!(std::fs::read(backup_dir.join("gallery.sqlite.age")).unwrap(), b"sealed-snapshot");
}

#[tokio::test]
async fn snapshot_keeps_the_captured_original_after_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let bytes = b"captured-original";
    let sha = gallery::ids::sha256_hex(bytes);
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &storage.backend_id,
        "media/captured.bin",
    )
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-captured', ?, ?, 'captured.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    std::fs::create_dir_all(dir.path().join("media")).unwrap();
    std::fs::write(dir.path().join("media/captured.bin"), bytes).unwrap();
    let snap = dir.path().join("snap.sqlite");
    gallery::backup::write_snapshot(&pool, &snap).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let later = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"later"),
        5,
        "application/octet-stream",
        "original",
        &storage.backend_id,
        "media/later.bin",
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM images WHERE id = 'img-captured'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-later', ?, ?, 'later.bin', 8, 8, 'application/octet-stream', 2, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"later"))
    .bind(&later)
    .bind(now + 1)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let captured = gallery::backup::snapshot_original_ids(&snap).await.unwrap();
    assert_eq!(captured, vec![blob_id]);
    assert_eq!(std::fs::read(dir.path().join("media/captured.bin")).unwrap(), bytes);
    let live: (String,) = sqlx::query_as("SELECT id FROM images")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live.0, "img-later");
}

#[tokio::test]
async fn a_resumed_restore_rechecks_a_copied_object_before_keeping_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let bytes = b"resume-original";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/resume/{sha}.bin");
    state.storage.put_bytes(&key, bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &state.storage.backend_id,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-resume', ?, ?, 'a.png', 8, 8, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let started = gallery::backup::start_backup(&state, &actor).await.unwrap();
    let backup_id = started["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let view = gallery::backup::get_backup(&state.pool, &actor, &backup_id).await.unwrap();
        if view["state"] == "succeeded" {
            ready = true;
            break;
        }
    }
    assert!(ready, "backup should finish");
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO restore_runs(id, backup_id, state, created_at, updated_at)
         VALUES('restore-resume', ?, 'queued', ?, ?)",
    )
    .bind(&backup_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    gallery::backup::run_restore(
        &state,
        &serde_json::json!({"scope": {"restore_id": "restore-resume", "backup_id": backup_id}}).to_string(),
    )
    .await
    .unwrap();
    let object = gallery::ids::sha256_hex(blob_id.as_bytes());
    let restored = dir.path().join("work/restore/restore-resume/media").join(&object);
    assert_eq!(std::fs::read(&restored).unwrap(), bytes);
    std::fs::write(&restored, b"tampered").unwrap();
    sqlx::query("UPDATE restore_runs SET state = 'queued' WHERE id = 'restore-resume'")
        .execute(&state.pool)
        .await
        .unwrap();
    gallery::backup::run_restore(
        &state,
        &serde_json::json!({"scope": {"restore_id": "restore-resume", "backup_id": backup_id}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&restored).unwrap(), bytes);
    let items: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM restore_items WHERE restore_id = 'restore-resume' AND state = 'verified'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(items.0, 1);
}

#[tokio::test]
async fn abort_restore_keeps_the_live_route_and_resume_requeues_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('workers_draining', '1')
         ON CONFLICT(key) DO UPDATE SET value = '1'",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let epoch: (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-live', 'succeeded', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-restore', 'restore', 'restore:restore-live', '{}', '{}', 'queued', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO restore_runs(id, backup_id, state, job_id, created_at, updated_at)
         VALUES('restore-live', 'bak-live', 'queued', 'job-restore', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO restore_items(restore_id, blob_id, state) VALUES('restore-live', 'blob-1', 'verified')",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let stopped = gallery::backup::abort_restore(&state, &actor, "restore-live")
        .await
        .unwrap();
    assert_eq!(stopped["state"], "canceled");
    assert_eq!(stopped["objects_verified"], 1);
    assert_eq!(stopped["routing_epoch"], epoch.0);
    let job: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-restore'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(job.0, "canceled");
    let again = gallery::backup::resume_restore(&state, &actor, "restore-live")
        .await
        .unwrap();
    assert_eq!(again["state"], "queued");
    assert_eq!(again["snapshot_at"], now);
    let requeued: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-restore'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(requeued.0, "queued");
    let live: (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(live.0, epoch.0);
}

#[tokio::test]
async fn restore_preview_lists_the_gap_without_starting_a_run() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let now = gallery::db::now_ms();
    let snapshot = now - 5_000;
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-plan', 'succeeded', ?, ?, ?)",
    )
    .bind(snapshot)
    .bind(snapshot)
    .bind(snapshot)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"new"),
        3,
        "application/octet-stream",
        "original",
        &storage.backend_id,
        "media/new.bin",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
         VALUES('img-new', ?, ?, 'new.bin', 8, 8, 'application/octet-stream', 'active', 1, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"new"))
    .bind(&blob)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let preview = gallery::backup::restore_plan(&pool, &actor, "bak-plan", None, None, false)
        .await
        .unwrap();
    assert_eq!(preview["mode"], "preview");
    assert_eq!(preview["unbacked_images"], 1);
    assert_eq!(preview["lost_images"][0]["id"], "img-new");
    assert!(preview["gap_ms"].as_i64().unwrap() >= 5_000);
    let refused = gallery::backup::restore_plan(&pool, &actor, "bak-plan", None, None, true)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("RESTORE_LOSS"));
    let dry = gallery::backup::restore_plan(
        &pool,
        &actor,
        "bak-plan",
        None,
        preview["loss_sha"].as_str(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(dry["mode"], "dry_run");
    assert_eq!(dry["parent_backup_id"], "bak-plan");
    let runs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM restore_runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(runs.0, 0);
}

#[tokio::test]
async fn restore_gap_lists_images_newer_than_the_last_backup() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let now = gallery::db::now_ms();
    let snapshot = now - 10_000;
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-old', 'succeeded', ?, ?, ?)",
    )
    .bind(snapshot)
    .bind(snapshot)
    .bind(snapshot)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    for (id, seq, created, label) in [
        ("img-old", 1_i64, snapshot - 1_000, "old"),
        ("img-new", 2, now, "new"),
    ] {
        let blob = gallery::storage::record_blob(
            &mut tx,
            &gallery::ids::sha256_hex(label.as_bytes()),
            label.len() as i64,
            "application/octet-stream",
            "original",
            &storage.backend_id,
            &format!("media/{id}.bin"),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
             VALUES(?, ?, ?, ?, 8, 8, 'application/octet-stream', 'active', ?, ?)",
        )
        .bind(id)
        .bind(gallery::ids::sha256_hex(label.as_bytes()))
        .bind(&blob)
        .bind(format!("{label}.bin"))
        .bind(seq)
        .bind(created)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let gap = gallery::backup::restore_gap(&pool, now, None).await.unwrap();
    assert_eq!(gap["backup_id"], "bak-old");
    assert_eq!(gap["gap_ms"], 10_000);
    assert_eq!(gap["unbacked_images"], 1);
    assert_eq!(gap["lost_images"][0]["id"], "img-new");
    assert!(gap["loss_sha"].as_str().unwrap().len() > 16);
    let refused = gallery::backup::require_loss_ack(&gap, None).unwrap_err();
    assert!(refused.to_string().contains("RESTORE_LOSS"));
    gallery::backup::require_loss_ack(&gap, gap["loss_sha"].as_str()).unwrap();
    let missing = gallery::backup::restore_gap(&pool, now, Some("bak-missing")).await;
    assert!(missing.unwrap_err().to_string().contains("NOT_FOUND"));
    let listed = gallery::backup::list_backups(&pool, &actor, None).await.unwrap();
    assert_eq!(listed["restore_gap"]["unbacked_images"], 1);
}

#[tokio::test]
async fn accept_stays_isolated_when_catalog_constraints_fail() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE username = 'admin'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0.clone(),
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let backend: (String,) = sqlx::query_as("SELECT active_backend_id FROM storage_route WHERE id = 1")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let purged = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"purged"),
        4,
        "image/jpeg",
        "original",
        &backend.0,
        "media/purged.bin",
    )
    .await
    .unwrap();
    let cover = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"cover"),
        4,
        "image/jpeg",
        "original",
        &backend.0,
        "media/cover.bin",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
         VALUES('img-purged', ?, ?, 'purged.jpg', 8, 8, 'image/jpeg', 'purged', 1, ?),
                ('img-cover', ?, ?, 'cover.jpg', 8, 8, 'image/jpeg', 'purged', 2, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"purged"))
    .bind(&purged)
    .bind(now)
    .bind(gallery::ids::sha256_hex(b"cover"))
    .bind(&cover)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO albums(id, name, cover_image_id, created_at) VALUES('album-1', 'A', 'img-cover', ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO album_images(album_id, image_id, sort_key, added_at) VALUES('album-1', 'img-purged', 10, ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO album_grants(album_id, actor_id, browse, edit) VALUES('album-1', ?, 0, 1)",
    )
    .bind(&owner.0)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let media = dir.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    let refused = gallery::backup::accept_isolated_restore(&pool, &actor, &media)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("RESTORE_CATALOG"));
    let barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(barrier.0, 1);
    let catalog = gallery::backup::catalog_constraints(&pool).await.unwrap();
    assert_eq!(catalog["ok"], false);
    assert_eq!(catalog["covers"][0]["image_id"], "img-cover");
    assert_eq!(catalog["members"][0]["image_id"], "img-purged");
    assert_eq!(catalog["grants"][0]["actor_id"], owner.0);
}

#[tokio::test]
async fn handoff_waits_for_a_backup_taken_after_writes_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let early = gallery::backup::finish_handoff(&state.pool, &actor, "").await.unwrap_err();
    assert!(early.to_string().contains("HANDOFF_ACCEPT"));
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let isolated = gallery::backup::finish_handoff(&state.pool, &actor, "").await.unwrap_err();
    assert!(isolated.to_string().contains("RESTORE_ISOLATED"));
    let opened = gallery::backup::accept_isolated_restore(&state.pool, &actor, &state.config.media_root())
        .await
        .unwrap();
    assert_eq!(opened["writes"], "open");
    let accepted_at = opened["accepted_at"].as_i64().unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-old', 'succeeded', ?, ?, ?)",
    )
    .bind(accepted_at - 1)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let waiting = gallery::backup::finish_handoff(&state.pool, &actor, "").await.unwrap_err();
    assert!(waiting.to_string().contains("HANDOFF_BACKUP"));
    let started = gallery::backup::start_backup(&state, &actor).await.unwrap();
    let backup_id = started["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let view = gallery::backup::get_backup(&state.pool, &actor, &backup_id).await.unwrap();
        if view["state"] == "succeeded" {
            ready = true;
            break;
        }
    }
    assert!(ready, "backup should finish");
    let missing_source = gallery::backup::finish_handoff(&state.pool, &actor, "").await.unwrap_err();
    assert!(missing_source.to_string().contains("HANDOFF_SOURCE"));
    let done = gallery::backup::finish_handoff(&state.pool, &actor, "source-ack").await.unwrap();
    assert_eq!(done["backup_id"], backup_id);
    assert_eq!(done["writes"], "open");
    assert!(done["snapshot_at"].as_i64().unwrap() >= accepted_at);
    let again = gallery::backup::finish_handoff(&state.pool, &actor, "source-ack").await.unwrap();
    assert_eq!(again["backup_id"], backup_id);
}

#[tokio::test]
async fn older_restore_point_opens_a_named_branch() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, branch_id, created_at, updated_at)
         VALUES('bak-old', 'succeeded', 100, 'main', ?, 500),
                ('bak-new', 'succeeded', 300, 'main', ?, 100)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let missing = gallery::backup::select_restore_branch(&pool, "bak-old", None)
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("BRANCH_REQUIRED"));
    let same = gallery::backup::select_restore_branch(&pool, "bak-old", Some("main"))
        .await
        .unwrap_err();
    assert!(same.to_string().contains("BRANCH_REQUIRED"));
    let side = gallery::backup::select_restore_branch(&pool, "bak-old", Some("side"))
        .await
        .unwrap();
    assert_eq!(side["parent_backup_id"], "bak-old");
    assert_eq!(side["branch_id"], "side");
    assert_eq!(side["branched"], true);
    assert_eq!(side["head_backup_id"], "bak-new");
    let head = gallery::backup::select_restore_branch(&pool, "bak-new", None)
        .await
        .unwrap();
    assert_eq!(head["parent_backup_id"], "bak-new");
    assert_eq!(head["branch_id"], "main");
    assert_eq!(head["branched"], false);
}

#[tokio::test]
async fn restore_point_stays_inside_one_library_and_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    sqlx::query("INSERT INTO schema_app(key, value) VALUES('instance_id', 'lib-a')")
        .execute(&pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, catalog_revision, routing_epoch, instance_id, branch_id, created_at, updated_at)
         VALUES('bak-foreign', 'succeeded', 900, 9, 9, 'lib-b', 'main', ?, 1),
                ('bak-low', 'succeeded', 200, 1, 2, 'lib-a', 'main', ?, 9),
                ('bak-high', 'succeeded', 200, 4, 7, 'lib-a', 'main', ?, 2)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let foreign = gallery::backup::select_restore_branch(&pool, "bak-foreign", None)
        .await
        .unwrap_err();
    assert!(foreign.to_string().contains("LIBRARY_MISMATCH"));
    let low = gallery::backup::select_restore_branch(&pool, "bak-low", None)
        .await
        .unwrap_err();
    assert!(low.to_string().contains("BRANCH_REQUIRED"));
    let high = gallery::backup::select_restore_branch(&pool, "bak-high", None)
        .await
        .unwrap();
    assert_eq!(high["head_backup_id"], "bak-high");
    assert_eq!(high["catalog_revision"], 4);
    assert_eq!(high["routing_epoch"], 7);
    assert_eq!(high["branched"], false);
    let branch = gallery::backup::select_restore_branch(&pool, "bak-low", Some("cutover"))
        .await
        .unwrap();
    assert_eq!(branch["parent_backup_id"], "bak-low");
    assert_eq!(branch["routing_epoch"], 2);
    assert_eq!(branch["head_backup_id"], "bak-high");
}

#[tokio::test]
async fn frozen_source_keeps_reads_and_refuses_gallery_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (created, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Before"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(created, StatusCode::CREATED, "{album}");
    let album_id = album["id"].as_str().unwrap();
    let (frozen_status, frozen) = h
        .json(Request::post("/api/v1/admin/source/freeze").body(Body::empty()).unwrap())
        .await;
    assert_eq!(frozen_status, StatusCode::OK, "{frozen}");
    assert_eq!(frozen["writes"], "readonly");
    let (entry_status, entry) = h
        .json(Request::get("/api/v1/library/entry").body(Body::empty()).unwrap())
        .await;
    assert_eq!(entry_status, StatusCode::OK, "{entry}");
    assert_eq!(entry["mode"], "readonly");
    assert_eq!(entry["reads"], "open");
    assert_eq!(entry["writes"], "blocked");
    let (listed, albums) = h
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(listed, StatusCode::OK, "{albums}");
    assert_eq!(albums["items"][0]["id"], album_id);
    let (open_album, body) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(open_album, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Before");
    let (blocked, problem) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"After"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(blocked, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "SOURCE_READONLY");
    let (listed_again, albums_again) = h
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(listed_again, StatusCode::OK);
    assert_eq!(albums_again["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn frozen_source_stays_readonly_until_handoff_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE username = 'admin'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let frozen = gallery::backup::freeze_source(&pool, &actor).await.unwrap();
    assert_eq!(frozen["writes"], "readonly");
    assert_eq!(frozen["ack"].as_str().unwrap().len(), 64);
    assert!(gallery::db::write_barrier(&pool).await.unwrap());
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, created_at, updated_at) VALUES('bak-open', 'queued', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    gallery::backup::abandon_unsealed(&pool, "bak-open").await.unwrap();
    assert!(gallery::upgrade::release_maintenance(&pool, "any").await.is_err());
    let refused = gallery::backup::accept_isolated_restore(&pool, &actor, dir.path()).await.unwrap_err();
    assert!(refused.to_string().contains("SOURCE_READONLY"));
    assert!(gallery::db::write_barrier(&pool).await.unwrap());
    assert!(gallery::db::source_readonly(&pool).await.unwrap());
}

#[tokio::test]
async fn drill_smoke_uses_a_disposable_sample_while_writes_stay_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let closed = gallery::backup::drill_smoke(&state, &actor).await.unwrap_err();
    assert!(closed.to_string().contains("DRILL_REQUIRED"));
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let report = gallery::backup::drill_smoke(&state, &actor).await.unwrap();
    assert_eq!(report["writes"], "isolated");
    assert_eq!(report["login"], "ok");
    assert_eq!(report["upload"], "ok");
    assert_eq!(report["thumbnail"], "ok");
    assert_eq!(report["original"], "ok");
    assert_eq!(report["archive"], "ok");
    assert_eq!(report["phone_download"], "ok");
    assert_eq!(report["queue_progress"]["done"], 1);
    assert_eq!(report["grant_revoked"], true);
    assert_eq!(report["sample_removed"], true);
    let images: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let albums: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM albums")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let jobs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE kind = 'drill_smoke'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let sessions: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NULL")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(images.0, 0);
    assert_eq!(albums.0, 0);
    assert_eq!(jobs.0, 0);
    assert_eq!(sessions.0, 0);
    assert!(gallery::db::write_barrier(&state.pool).await.unwrap());
}

#[tokio::test]
async fn restored_runtime_drops_uploads_claims_and_leases() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"kept"),
        4,
        "application/octet-stream",
        "original",
        &storage.backend_id,
        "media/kept.bin",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-kept', ?, ?, 'kept.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"kept"))
    .bind(&blob)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO albums(id, name, created_at) VALUES('album-1', 'A', ?)")
        .bind(now)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sessions(id, actor_id, token_sha256, csrf_token, expires_at, created_at)
         VALUES('sess-1', ?, 'tok', 'csrf', ?, ?)",
    )
    .bind(&owner.0)
    .bind(now + 10_000)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upload_batches(id, album_id, actor_id, state, created_at, updated_at)
         VALUES('batch-1', 'album-1', ?, 'running', ?, ?)",
    )
    .bind(&owner.0)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upload_items(id, batch_id, client_item_id, original_name, declared_size, state)
         VALUES('item-open', 'batch-1', 'c1', 'a.bin', 4, 'receiving'),
                ('item-done', 'batch-1', 'c2', 'b.bin', 4, 'committed')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO download_claims(id, actor_id, album_id, image_id, expires_at, created_at)
         VALUES('claim-1', ?, 'album-1', 'img-kept', ?, ?)",
    )
    .bind(&owner.0)
    .bind(now + 60_000)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, lease_token, lease_until, available_at, created_at, updated_at)
         VALUES('job-run', 'generate_variant', 'dedupe-run', '{}', '{}', 'running', 'lease', ?, ?, ?, ?)",
    )
    .bind(now + 30_000)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, lease_token, lease_until, available_at, created_at, updated_at)
         VALUES('job-wait', 'caption_image', 'dedupe-wait', '{}', '{}', 'queued', 'old-lease', ?, ?, ?, ?)",
    )
    .bind(now + 30_000)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let report = gallery::backup::rebuild_restored_runtime(&pool, "restore-1", "bak-1")
        .await
        .unwrap();
    assert_eq!(report["uploads_canceled"], 1);
    assert_eq!(report["claims_expired"], 1);
    assert!(report["leases_cleared"].as_i64().unwrap() >= 1);
    assert!(report["sessions_revoked"].as_i64().unwrap() >= 1);
    let item: (String, Option<String>) = sqlx::query_as(
        "SELECT state, failure_code FROM upload_items WHERE id = 'item-open'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(item.0, "canceled");
    assert_eq!(item.1.as_deref(), Some("RESTORED"));
    let done: (String,) = sqlx::query_as("SELECT state FROM upload_items WHERE id = 'item-done'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(done.0, "committed");
    let batch: (String,) = sqlx::query_as("SELECT state FROM upload_batches WHERE id = 'batch-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(batch.0, "canceled");
    let claim: (i64,) = sqlx::query_as("SELECT expires_at FROM download_claims WHERE id = 'claim-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(claim.0 < gallery::db::now_ms());
    let job: (String, Option<String>) =
        sqlx::query_as("SELECT state, lease_token FROM jobs WHERE id = 'job-run'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(job.0, "queued");
    assert!(job.1.is_none());
}

#[tokio::test]
async fn restored_runtime_rebuilds_missing_variants_and_holds_agent_turns() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::versions::ensure_default_profile(&pool).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let missing = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"missing-original"),
        4,
        "image/jpeg",
        "original",
        &storage.backend_id,
        "media/missing.bin",
    )
    .await
    .unwrap();
    let present = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"present-original"),
        4,
        "image/jpeg",
        "original",
        &storage.backend_id,
        "media/present.bin",
    )
    .await
    .unwrap();
    let thumb = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"present-thumb"),
        4,
        "image/jpeg",
        "variant",
        &storage.backend_id,
        "media/present-thumb.bin",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-missing', ?, ?, 'missing.jpg', 8, 8, 'image/jpeg', 1, ?),
                ('img-present', ?, ?, 'present.jpg', 8, 8, 'image/jpeg', 2, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"missing-original"))
    .bind(&missing)
    .bind(now)
    .bind(gallery::ids::sha256_hex(b"present-original"))
    .bind(&present)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO image_variants(id, image_id, profile_revision, spec_id, blob_id, state)
         VALUES('var-thumb', 'img-present', 1, 'thumb', ?, 'published')",
    )
    .bind(&thumb)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO variant_heads(image_id, spec_id, variant_id, published_at)
         VALUES('img-present', 'thumb', 'var-thumb', ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-retire', 'storage_retire', 'dedupe-retire', '{}', '{}', 'queued', ?, ?, ?),
                ('job-thumb-done', 'generate_variant', 'generate_variant:img-present:1:thumb', '{}', '{}', 'succeeded', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, created_at, updated_at)
         VALUES('agent-1', ?, '{}', 'active', ?, ?)",
    )
    .bind(&owner.0)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at)
         VALUES('turn-live', 'agent-1', 1, 'running', ?),
                ('turn-done', 'agent-1', 1, 'completed', ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_actions(id, session_id, turn_id, state, created_at)
         VALUES('act-1', 'agent-1', 'turn-live', 'approved', ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let report = gallery::backup::rebuild_restored_runtime(&pool, "restore-variants", "bak-variants")
        .await
        .unwrap();
    assert_eq!(report["variants_queued"], 7);
    assert_eq!(report["sessions_reviewed"], 1);
    assert_eq!(report["turns_reviewed"], 1);
    let ready: (String,) = sqlx::query_as("SELECT value FROM schema_app WHERE key = 'search_index_ready'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ready.0, "0");
    let search: (String,) = sqlx::query_as(
        "SELECT state FROM jobs WHERE dedupe_key = 'rebuild_search:restore:restore-variants'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(search.0, "queued");
    let thumb_job: (String,) = sqlx::query_as(
        "SELECT state FROM jobs WHERE dedupe_key = 'generate_variant:img-present:1:thumb'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(thumb_job.0, "succeeded");
    let missing_thumb: (String,) = sqlx::query_as(
        "SELECT state FROM jobs WHERE dedupe_key = 'generate_variant:img-missing:1:thumb'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(missing_thumb.0, "queued");
    let retire: (String, String) = sqlx::query_as(
        "SELECT state, failure_code FROM jobs WHERE id = 'job-retire'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retire.0, "failed");
    assert_eq!(retire.1, "NEEDS_REVIEW");
    let session: (String,) = sqlx::query_as("SELECT state FROM agent_sessions WHERE id = 'agent-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(session.0, "needs_reconcile");
    let live: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-live'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live.0, "needs_reconcile");
    let done: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-done'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(done.0, "completed");
    let action: (String,) = sqlx::query_as("SELECT state FROM agent_actions WHERE id = 'act-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(action.0, "needs_reconcile");
}

#[tokio::test]
async fn purge_waits_while_a_backup_pin_holds_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let bytes = b"pinned-original";
    state.storage.put_bytes("media/pinned.bin", bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(bytes),
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &state.storage.backend_id,
        "media/pinned.bin",
    )
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
         VALUES('img-pin', ?, ?, 'pinned.bin', 8, 8, 'application/octet-stream', 'purging', 9001, ?)",
    )
    .bind(gallery::ids::sha256_hex(bytes))
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, manifest_digest, created_at, updated_at)
         VALUES('bak-pin', 'running', 'sealed', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES('bak-pin', ?)")
        .bind(&blob_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let held = gallery::workers::execute(
        &state,
        "purge_media",
        r#"{"scope":{"image_id":"img-pin"}}"#,
        "job-pin",
        "lease-pin",
    )
    .await;
    assert!(held.is_err());
    assert!(dir.path().join("media").join("media").join("pinned.bin").is_file());
    let still: (String,) = sqlx::query_as("SELECT state FROM images WHERE id = 'img-pin'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(still.0, "purging");
    sqlx::query("DELETE FROM backup_pins WHERE backup_id = 'bak-pin'")
        .execute(&state.pool)
        .await
        .unwrap();
    gallery::workers::execute(
        &state,
        "purge_media",
        r#"{"scope":{"image_id":"img-pin"}}"#,
        "job-pin",
        "lease-pin",
    )
        .await
        .unwrap();
    assert!(!dir.path().join("media").join("media").join("pinned.bin").is_file());
    let purged: (String,) = sqlx::query_as("SELECT state FROM images WHERE id = 'img-pin'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(purged.0, "purged");
}

#[tokio::test]
async fn sealed_backup_resumes_copy_without_a_new_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let bytes = b"sealed-original";
    let sha = gallery::ids::sha256_hex(bytes);
    state.storage.put_bytes("media/sealed.bin", bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &state.storage.backend_id,
        "media/sealed.bin",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let object = gallery::ids::sha256_hex(blob_id.as_bytes());
    let manifest = serde_json::json!({
        "backup_id": "bak-resume",
        "format_version": 1,
        "snapshot_at": 10,
        "catalog_revision": 1,
        "items": [{"blob_id": blob_id, "sha256": sha, "size": bytes.len(), "object": object}]
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let digest = gallery::ids::sha256_hex(&manifest_bytes);
    let dest = state.config.backup_root().join("bak-resume");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("manifest.json"), &manifest_bytes).unwrap();
    std::fs::write(dest.join("gallery.sqlite"), b"sealed-snapshot").unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, manifest_digest, created_at, updated_at)
         VALUES('bak-resume', 'running', ?, ?, ?)",
    )
    .bind(&digest)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    gallery::backup::run_backup(&state, r#"{"scope":{"backup_id":"bak-resume"}}"#)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest.join("gallery.sqlite")).unwrap(), b"sealed-snapshot");
    assert!(!gallery::db::write_barrier(&state.pool).await.unwrap());
    let copied = gallery::backup::age_decrypt_bytes(
        &state.config.keys_root(),
        &std::fs::read(dest.join(&object)).unwrap(),
    )
    .unwrap();
    assert_eq!(copied, bytes);
    let run: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-resume'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(run.0, "succeeded");
}

#[tokio::test]
async fn verified_backup_object_is_reused_until_its_file_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let bytes = b"original-bytes";
    let sha = gallery::ids::sha256_hex(bytes);
    state.storage.put_bytes("media/original.bin", bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &state.storage.backend_id,
        "media/original.bin",
    )
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, manifest_digest, created_at, updated_at)
         VALUES('bak-reuse', 'running', 'sealed', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    let object_name = gallery::ids::sha256_hex(blob_id.as_bytes());
    let ciphertext = gallery::backup::age_encrypt_bytes(&state.config.keys_root(), bytes).unwrap();
    let ciphertext_sha = gallery::ids::sha256_hex(&ciphertext);
    sqlx::query(
        "INSERT INTO backup_items(backup_id, blob_id, ciphertext_key, plaintext_sha256, ciphertext_sha256, size, state)
         VALUES('bak-reuse', ?, ?, ?, ?, ?, 'verified')",
    )
    .bind(&blob_id)
    .bind(&object_name)
    .bind(&sha)
    .bind(&ciphertext_sha)
    .bind(bytes.len() as i64)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let dest = dir.path().join("backups").join("bak-reuse");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join(&object_name), &ciphertext).unwrap();
    std::fs::remove_file(dir.path().join("media").join("media").join("original.bin")).unwrap();
    let reused = gallery::backup::copy_backup_object(
        &state,
        "bak-reuse",
        &dest,
        &blob_id,
        bytes.len() as i64,
        &sha,
    )
    .await
    .unwrap();
    assert_eq!(reused, "reused");
    std::fs::remove_file(dest.join(&object_name)).unwrap();
    state
        .storage
        .put_bytes("media/original.bin", bytes, "application/octet-stream")
        .await
        .unwrap();
    let copied = gallery::backup::copy_backup_object(
        &state,
        "bak-reuse",
        &dest,
        &blob_id,
        bytes.len() as i64,
        &sha,
    )
    .await
    .unwrap();
    assert_eq!(copied, "copied");
    let attempt: (String,) = sqlx::query_as(
        "SELECT ciphertext_key FROM backup_items WHERE backup_id = 'bak-reuse' AND blob_id = ?",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_ne!(attempt.0, object_name);
    std::fs::write(dest.join(&object_name), b"partial-upload").unwrap();
    let retried = gallery::backup::copy_backup_object(
        &state,
        "bak-reuse",
        &dest,
        &blob_id,
        bytes.len() as i64,
        &sha,
    )
    .await
    .unwrap();
    assert_eq!(retried, "retried");
    let same_key: (String,) = sqlx::query_as(
        "SELECT ciphertext_key FROM backup_items WHERE backup_id = 'bak-reuse' AND blob_id = ?",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(same_key.0, object_name);
    let restored = gallery::backup::age_decrypt_bytes(
        &state.config.keys_root(),
        &std::fs::read(dest.join(object_name)).unwrap(),
    )
    .unwrap();
    assert_eq!(restored, bytes);
}

#[tokio::test]
async fn sealed_copy_can_be_abandoned_without_a_later_success() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"pin"),
        3,
        "application/octet-stream",
        "original",
        &storage.backend_id,
        "pins/blob.bin",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, manifest_digest, created_at, updated_at)
         VALUES('bak-copy', 'running', 'sealed', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES('bak-copy', ?)")
        .bind(&blob_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let staging = dir.path().join("backups").join("bak-copy");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("partial.bin"), b"partial").unwrap();
    gallery::backup::abandon_incomplete_copy(&pool, "bak-copy", &dir.path().join("backups"))
        .await
        .unwrap();
    let state: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-copy'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state.0, "abandoned");
    let pins: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-copy'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pins.0, 0);
    assert!(!staging.exists());
    assert!(!gallery::db::write_barrier(&pool).await.unwrap());
    assert!(gallery::backup::commit_sealed_success(&pool, "bak-copy").await.is_err());
    let still: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-copy'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still.0, "abandoned");
}

#[tokio::test]
async fn interrupted_capture_stays_unusable_until_restart_resumes_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('workers_draining', '1')
         ON CONFLICT(key) DO UPDATE SET value = '1'",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES('blob-pin', 'aa', 4, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, lease_token, lease_until, created_at, updated_at)
         VALUES('job-snap', 'backup', 'backup:bak-snap', '{}', '{}', 'running', ?, 'lease-snap', ?, ?, ?),
                ('job-copy', 'backup', 'backup:bak-copy', '{}', '{}', 'running', ?, 'lease-copy', ?, ?, ?)",
    )
    .bind(now)
    .bind(now + 60_000)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now + 60_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-ok', 'succeeded', ?, ?, ?),
                ('bak-freeze', 'queued', NULL, ?, ?)",
    )
    .bind(now - 1_000)
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, job_id, created_at, updated_at)
         VALUES('bak-snap', 'running', 'job-snap', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let manifest = br#"{"backup_id":"bak-copy","items":[]}"#;
    let digest = gallery::ids::sha256_hex(manifest);
    sqlx::query(
        "INSERT INTO backup_runs(id, state, job_id, snapshot_at, manifest_digest, created_at, updated_at)
         VALUES('bak-copy', 'running', 'job-copy', ?, ?, ?, ?)",
    )
    .bind(now - 500)
    .bind(&digest)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES('bak-copy', 'blob-pin')")
        .execute(&state.pool)
        .await
        .unwrap();
    let snap_dir = state.config.backup_root().join("bak-snap");
    std::fs::create_dir_all(&snap_dir).unwrap();
    std::fs::write(snap_dir.join("gallery.sqlite"), b"partial-snapshot").unwrap();
    let copy_dir = state.config.backup_root().join("bak-copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    std::fs::write(copy_dir.join("manifest.json"), manifest).unwrap();
    let resumed = gallery::backup::recover_interrupted_backups(&state.pool).await.unwrap();
    assert_eq!(resumed, vec!["bak-copy".to_string(), "bak-freeze".to_string(), "bak-snap".to_string()]);
    assert!(gallery::db::write_barrier(&state.pool).await.unwrap());
    for id in ["bak-freeze", "bak-snap", "bak-copy"] {
        assert!(gallery::backup::restore_gap(&state.pool, now, Some(id)).await.is_err());
        let state_name: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = ?")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(state_name.0, "queued");
    }
    let ready = gallery::backup::restore_gap(&state.pool, now, Some("bak-ok")).await.unwrap();
    assert_eq!(ready["backup_id"], "bak-ok");
    let freeze_job: (String,) = sqlx::query_as(
        "SELECT j.state FROM jobs j JOIN backup_runs b ON b.job_id = j.id WHERE b.id = 'bak-freeze'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(freeze_job.0, "queued");
    let snap_job: (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT lease_token, lease_until FROM jobs WHERE id = 'job-snap'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(snap_job.0.is_none());
    assert!(snap_job.1.is_none());
    let pins: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-copy'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(pins.0, 1);
    assert_eq!(std::fs::read(snap_dir.join("gallery.sqlite")).unwrap(), b"partial-snapshot");
    gallery::backup::run_backup(&state, r#"{"scope":{"backup_id":"bak-copy"}}"#)
        .await
        .unwrap();
    let copied: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-copy'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(copied.0, "succeeded");
    assert!(copy_dir.join("COMPLETE").is_file());
    let pins_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-copy'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(pins_after.0, 0);
    assert!(gallery::db::write_barrier(&state.pool).await.unwrap());
    let still: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-snap'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(still.0, "queued");
}

#[tokio::test]
async fn unsealed_backup_keeps_the_write_barrier_until_abandoned() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query("INSERT INTO backup_runs(id, state, created_at, updated_at) VALUES('bak-open', 'running', ?, ?)")
        .bind(now)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    std::fs::remove_dir_all(state.config.backup_root()).unwrap();
    std::fs::write(state.config.backup_root(), b"not-a-directory").unwrap();
    let failed = gallery::backup::run_backup(
        &state,
        r#"{"scope":{"backup_id":"bak-open"}}"#,
    )
    .await;
    assert!(failed.is_err());
    assert!(gallery::db::write_barrier(&state.pool).await.unwrap());
    gallery::backup::abandon_unsealed(&state.pool, "bak-open").await.unwrap();
    assert!(!gallery::db::write_barrier(&state.pool).await.unwrap());
    let state_name: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-open'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(state_name.0, "abandoned");
    sqlx::query(
        "INSERT INTO backup_runs(id, state, manifest_digest, created_at, updated_at)
         VALUES('bak-sealed', 'running', 'abc', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    assert!(gallery::backup::abandon_unsealed(&state.pool, "bak-sealed").await.is_err());
}

#[tokio::test]
async fn backup_and_restore_local() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    let (s, headers, body) = h
        .call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"admin","password":"owner-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let set_cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    h.cookie = set_cookie
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("gallery_session=")
        .unwrap()
        .to_string();
    h.csrf = serde_json::from_slice::<Value>(&body).unwrap()["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, v) = h
        .json(
            Request::post("/api/v1/admin/backups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let id = v["id"].as_str().unwrap().to_string();
    let mut ok = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, st) = h
            .json(
                Request::get(format!("/api/v1/admin/backups/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        if st["state"] == "succeeded" {
            ok = true;
            break;
        }
    }
    assert!(ok, "backup should succeed");
}

#[tokio::test]
async fn restore_rebinds_originals_onto_a_new_backend_when_the_old_path_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let bytes = b"move-with-the-backup";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/move/{sha}.bin");
    state.storage.put_bytes(&key, bytes, "image/png").await.unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &state.storage.backend_id,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-move', ?, ?, 'a.png', 8, 8, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, created_at)
         VALUES('user-move', 'mover', 'hash', 'collaborator', ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO albums(id, name, created_at) VALUES('album-move', 'Move', ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO album_images(album_id, image_id, sort_key, added_at)
         VALUES('album-move', 'img-move', 1, ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO album_grants(album_id, actor_id, browse, edit, view_original)
         VALUES('album-move', 'user-move', 1, 1, 1)",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let live_backend = state.storage.backend_id.clone();
    let started = gallery::backup::start_backup(&state, &actor).await.unwrap();
    let backup_id = started["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let view = gallery::backup::get_backup(&state.pool, &actor, &backup_id).await.unwrap();
        if view["state"] == "succeeded" {
            ready = true;
            break;
        }
    }
    assert!(ready, "backup should finish");
    let live_file = state.config.media_root().join(&key);
    std::fs::remove_file(&live_file).unwrap();
    let live_epoch: (i64, i64) =
        sqlx::query_as("SELECT routing_epoch, write_barrier FROM storage_route WHERE id = 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO restore_runs(id, backup_id, state, created_at, updated_at)
         VALUES('restore-move', ?, 'queued', ?, ?)",
    )
    .bind(&backup_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    gallery::backup::run_restore(
        &state,
        &serde_json::json!({"scope": {"restore_id": "restore-move", "backup_id": backup_id}}).to_string(),
    )
    .await
    .unwrap();
    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("work/restore/restore-move/report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["target_backend_id"], "restore-target");
    assert_eq!(report["locations_rebuilt"], 1);
    assert_eq!(report["old_backend_id"], live_backend);
    assert_eq!(report["old_path_used"], false);
    assert_eq!(report["writes"], "isolated");
    assert_eq!(report["catalog_ok"], true);
    assert_eq!(report["smoke"]["ok"], true);
    assert_eq!(report["smoke"]["checked"], 1);
    assert!(report["rto_ms"].as_i64().unwrap() >= 0);
    let exported = gallery::backup::export_restore_report(&state, &actor, "restore-move")
        .await
        .unwrap();
    assert_eq!(exported["backup_id"], backup_id);
    assert_eq!(exported["snapshot_at"], report["snapshot_at"]);
    assert_eq!(exported["integrity"], "ok");
    assert_eq!(exported["originals_verified"], 1);
    assert!(exported["originals_missing"].as_array().unwrap().is_empty());
    assert_eq!(exported["target_backend_id"], "restore-target");
    assert_eq!(exported["search_job"].as_str().unwrap().is_empty(), false);
    assert_eq!(exported["smoke"]["ok"], true);
    assert!(exported["rto_ms"].as_i64().unwrap() >= 0);
    assert!(!exported.to_string().contains(dir.path().to_str().unwrap()));
    let collaborator = gallery::auth::Actor {
        id: "user-move".into(),
        username: "mover".into(),
        role: "collaborator".into(),
        must_change_password: false,
        revision: 1,
    };
    assert!(gallery::backup::export_restore_report(&state, &collaborator, "restore-move")
        .await
        .is_err());
    let isolated = gallery::db::connect(&dir.path().join("work/restore/restore-move/gallery.sqlite"))
        .await
        .unwrap();
    let object = gallery::ids::sha256_hex(blob_id.as_bytes());
    let target = gallery::storage::open_by_id(&isolated, "restore-target").await.unwrap();
    assert_eq!(target.get_bytes(&object).await.unwrap(), bytes);
    let old = gallery::storage::open_by_id(&isolated, &live_backend).await.unwrap();
    assert!(old.get_bytes(&key).await.is_err());
    let route: (String, i64) =
        sqlx::query_as("SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1")
            .fetch_one(&isolated)
            .await
            .unwrap();
    assert_eq!(route.0, "restore-target");
    assert_eq!(route.1, 1);
    let grant: (i64, i64, i64) = sqlx::query_as(
        "SELECT browse, edit, view_original FROM album_grants
         WHERE album_id = 'album-move' AND actor_id = 'user-move'",
    )
    .fetch_one(&isolated)
    .await
    .unwrap();
    assert_eq!(grant, (1, 1, 1));
    let live: (String, i64, i64) = sqlx::query_as(
        "SELECT active_backend_id, routing_epoch, write_barrier FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(live.0, live_backend);
    assert_eq!(live.1, live_epoch.0);
    assert_eq!(live.2, live_epoch.1);
    assert!(!live_file.exists());
}

#[tokio::test]
async fn restore_checks_signature_and_object_hash() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let media = state.media().await.unwrap();
    let bytes = b"restore-me-bytes";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img1/{sha}.bin");
    media.put_bytes(&key, bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &media.backend_id,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-restore', ?, ?, 'a.png', 8, 8, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (session_token, _) = gallery::auth::issue_session(&state.pool, &actor, 3_600_000)
        .await
        .unwrap();
    let (source_epoch,): (i64,) =
        sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let started = gallery::backup::start_backup(&state, &actor).await.unwrap();
    let backup_id = started["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let view = gallery::backup::get_backup(&state.pool, &actor, &backup_id).await.unwrap();
        if view["state"] == "succeeded" {
            ready = true;
            break;
        }
    }
    assert!(ready, "backup should finish");
    let now = gallery::db::now_ms();
    sqlx::query("INSERT INTO restore_runs(id, backup_id, state, created_at, updated_at) VALUES('restore-1', ?, 'queued', ?, ?)")
        .bind(&backup_id)
        .bind(now)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
    gallery::backup::run_restore(
        &state,
        &serde_json::json!({"scope": {"restore_id": "restore-1", "backup_id": backup_id}}).to_string(),
    )
    .await
    .unwrap();
    let object = gallery::ids::sha256_hex(blob_id.as_bytes());
    let restored = std::fs::read(dir.path().join("work/restore/restore-1/media").join(&object)).unwrap();
    assert_eq!(restored, bytes);
    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("work/restore/restore-1/report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["integrity"], "ok");
    assert_eq!(report["images"], 1);
    assert_eq!(report["routing_epoch"], source_epoch + 1);
    assert_eq!(report["sessions_revoked"], 1);
    assert_eq!(report["writes"], "isolated");
    assert_eq!(report["originals_verified"], 1);
    assert_eq!(report["catalog_ok"], true);
    assert!(report["originals_missing"].as_array().unwrap().is_empty());
    assert!(report["snapshot_at"].as_i64().unwrap() > 0);
    let isolated = gallery::db::connect(&dir.path().join("work/restore/restore-1/gallery.sqlite"))
        .await
        .unwrap();
    let media_dir = dir.path().join("work/restore/restore-1/media");
    std::fs::remove_file(media_dir.join(&object)).unwrap();
    let refused = gallery::backup::accept_isolated_restore(&isolated, &actor, &media_dir)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("RESTORE_MISSING"));
    let held: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&isolated)
        .await
        .unwrap();
    assert_eq!(held.0, 1);
    std::fs::write(media_dir.join(&object), &restored).unwrap();
    let opened = gallery::backup::accept_isolated_restore(&isolated, &actor, &media_dir)
        .await
        .unwrap();
    assert_eq!(opened["writes"], "open");
    assert_eq!(opened["originals_verified"], 1);
    assert_eq!(opened["next"], "backup");
    let opened_barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&isolated)
        .await
        .unwrap();
    assert_eq!(opened_barrier.0, 0);
    let (revoked,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sessions WHERE revoked_at IS NULL")
            .fetch_one(&isolated)
            .await
            .unwrap();
    assert_eq!(revoked, 0);
    assert!(gallery::auth::lookup_session(&state.pool, &session_token)
        .await
        .unwrap()
        .is_some());
    std::fs::write(dir.path().join("backups").join(&backup_id).join("COMPLETE"), "bad\nbad").unwrap();
    sqlx::query("INSERT INTO restore_runs(id, backup_id, state, created_at, updated_at) VALUES('restore-2', ?, 'queued', ?, ?)")
        .bind(&backup_id)
        .bind(now)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
    let err = gallery::backup::run_restore(
        &state,
        &serde_json::json!({"scope": {"restore_id": "restore-2", "backup_id": backup_id}}).to_string(),
    )
    .await;
    assert!(err.is_err());
    let state_row: (String,) = sqlx::query_as("SELECT state FROM restore_runs WHERE id = 'restore-2'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(state_row.0, "failed");
}

#[tokio::test]
async fn idempotent_album_and_cursor_and_lost_lease() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let body = r#"{"name":"Same"}"#;
    let first = Request::post("/api/v1/albums")
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", "album-1")
        .body(Body::from(body))
        .unwrap();
    let (s1, a1) = h.json(first).await;
    let (s2, a2) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "album-1")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::CREATED);
    assert_eq!(a1["id"], a2["id"]);
    let (s3, conflict) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "album-1")
                .body(Body::from(r#"{"name":"Other"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(s3, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "IDEMPOTENCY_CONFLICT");

    let album_id = a1["id"].as_str().unwrap();
    for (name, pixels) in [("a.png", 8u32), ("b.png", 9)] {
        let png = png_bytes(pixels, pixels, false);
        let (_, batch) = h
            .json(
                Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"items":[{"client_item_id":name,"name":name,"size":png.len()}]})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        let item = batch["items"][0]["upload_item_id"].as_str().unwrap();
        let batch_id = batch["id"].as_str().unwrap();
        let (s, committed) = h
            .json(
                Request::put(format!("/api/v1/uploads/{batch_id}/items/{item}/body"))
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(png))
                    .unwrap(),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{committed}");
    }
    let (s, page1) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images?limit=1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let cursor = page1["next_cursor"].as_str().expect("second page");
    let (s, page2) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images?limit=1&cursor={cursor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_ne!(page1["items"][0]["id"], page2["items"][0]["id"]);
    let (s, stale) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images?cursor=not-a-cursor"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(stale["code"], "CURSOR_STALE");

    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().join("lease");
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, lease_token, lease_until, attempt_no, created_at, updated_at)
         VALUES('job-lease', 'reconcile', 'dedupe-lease', '{}', '{}', 'running', ?, 'lease-1', ?, 1, ?, ?)",
    )
    .bind(now)
    .bind(now + 60_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let published = gallery::jobs::commit_result(&state.pool, "job-lease", "other-lease", true, None)
        .await
        .unwrap();
    assert!(!published);
    let job = gallery::jobs::get(&state.pool, "job-lease").await.unwrap().unwrap();
    assert_eq!(job.state, "running");
}

#[tokio::test]
async fn expired_lease_cannot_publish_and_attempts_stop() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, lease_token, lease_until, attempt_no, max_attempts, created_at, updated_at)
         VALUES('job-stale', 'reconcile', 'dedupe-stale', '{}', '{}', 'running', ?, 'lease-stale', ?, 1, 3, ?, ?)",
    )
    .bind(now)
    .bind(now - 1_000)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let published = gallery::jobs::commit_result(&pool, "job-stale", "lease-stale", true, None)
        .await
        .unwrap();
    assert!(!published);
    let job = gallery::jobs::get(&pool, "job-stale").await.unwrap().unwrap();
    assert_eq!(job.state, "running");
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, attempt_no, max_attempts, created_at, updated_at)
         VALUES('job-spent', 'reconcile', 'dedupe-spent', '{}', '{}', 'queued', ?, 3, 3, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    gallery::jobs::reclaim_expired(&pool).await.unwrap();
    let spent = gallery::jobs::get(&pool, "job-spent").await.unwrap().unwrap();
    assert_eq!(spent.state, "failed");
    assert_eq!(spent.failure.as_deref(), Some("ATTEMPTS_EXHAUSTED"));
    let stale = gallery::jobs::get(&pool, "job-stale").await.unwrap().unwrap();
    assert_eq!(stale.state, "retry_wait");
}

#[tokio::test]
async fn local_storage_cutover_and_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let media = state.media().await.unwrap();
    let original = media.backend_id.clone();
    let bytes = b"hello-bytes-for-move";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img1/{sha}.bin");
    media.put_bytes(&key, bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &original,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-1', ?, ?, 'a.png', 8, 8, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let target = dir.path().join("other-media");
    let started = gallery::migrate_storage::start(
        &state,
        &actor,
        gallery::storage::BackendConfig::Local { root: target.clone() },
    )
    .await
    .unwrap();
    let migration_id = started["id"].as_str().unwrap().to_string();
    let ready = wait_migration_stage(&state, &actor, &migration_id, "ready").await;
    assert_eq!(ready["next_action"], "cutover");
    assert_eq!(ready["active_backend_id"], original);
    assert_eq!(state.media().await.unwrap().backend_id, original);
    let listed = gallery::migrate_storage::items(&state.pool, &actor, &migration_id, 50, None)
        .await
        .unwrap();
    assert_eq!(listed["items"][0]["state"], "verified");
    assert!(listed["items"][0].get("object_key").is_none());
    assert!(target.join(&key).is_file());
    let switched = gallery::migrate_storage::cutover(
        &state,
        &actor,
        &migration_id,
        ready["revision"].as_i64().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(switched["stage"], "protected");
    let active = state.media().await.unwrap();
    assert_ne!(active.backend_id, original);
    assert_eq!(active.get_bytes(&key).await.unwrap(), bytes);
    let blocked = gallery::migrate_storage::retire(&state, &actor, &migration_id, true, None, None)
        .await
        .unwrap();
    let blockers = blocked["blockers"].as_array().unwrap();
    assert!(blockers.iter().any(|item| item == "protection period"));
    assert!(blockers.iter().any(|item| item == "post-cutover backups"));
    let refused = gallery::migrate_storage::retire(
        &state,
        &actor,
        &migration_id,
        false,
        Some(blocked["revision"].as_i64().unwrap()),
        blocked["preview_sha"].as_str(),
    )
    .await;
    assert!(refused.unwrap_err().to_string().contains("RETIRE_BLOCKED"));
    let rolled = gallery::migrate_storage::rollback(&state, &actor, &migration_id)
        .await
        .unwrap();
    assert_eq!(rolled["stage"], "protected");
    let restored = state.media().await.unwrap();
    assert_eq!(restored.backend_id, original);
    assert_eq!(restored.get_bytes(&key).await.unwrap(), bytes);
}

#[tokio::test]
async fn rollback_copies_new_originals_and_stops_when_one_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source_id = state.media().await.unwrap().backend_id.clone();
    let bytes = b"already-migrated";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-old/{sha}.bin");
    state.media().await.unwrap().put_bytes(&key, bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &source_id,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-old', ?, ?, 'old.png', 8, 8, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let target = dir.path().join("rolled-media");
    let started = gallery::migrate_storage::start(
        &state,
        &actor,
        gallery::storage::BackendConfig::Local { root: target.clone() },
    )
    .await
    .unwrap();
    let migration_id = started["id"].as_str().unwrap().to_string();
    let ready = wait_migration_stage(&state, &actor, &migration_id, "ready").await;
    gallery::migrate_storage::cutover(&state, &actor, &migration_id, ready["revision"].as_i64().unwrap())
        .await
        .unwrap();
    let active = state.media().await.unwrap();
    let fresh = b"written-after-cutover";
    let fresh_sha = gallery::ids::sha256_hex(fresh);
    let fresh_key = format!("media/cd/img-new/{fresh_sha}.bin");
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let fresh_blob = gallery::storage::record_blob(
        &mut tx,
        &fresh_sha,
        fresh.len() as i64,
        "image/png",
        "original",
        &active.backend_id,
        &fresh_key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-new', ?, ?, 'new.png', 8, 8, 'image/png', 2, ?)",
    )
    .bind(&fresh_sha)
    .bind(&fresh_blob)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let blocked = gallery::migrate_storage::rollback(&state, &actor, &migration_id).await;
    assert!(blocked.unwrap_err().to_string().contains("ROLLBACK_MISSING_MEDIA"));
    let route: (String, i64) = sqlx::query_as(
        "SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(route.0, active.backend_id);
    assert_eq!(route.1, 1);
    active.put_bytes(&fresh_key, fresh, "image/png").await.unwrap();
    let rolled = gallery::migrate_storage::rollback(&state, &actor, &migration_id)
        .await
        .unwrap();
    assert_eq!(rolled["stage"], "protected");
    assert!(dir.path().join("media").join(&fresh_key).is_file());
    let restored = state.media().await.unwrap();
    assert_eq!(restored.backend_id, source_id);
    assert_eq!(restored.get_bytes(&fresh_key).await.unwrap(), fresh);
    let barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(barrier.0, 0);
}

async fn wait_migration_stage(
    state: &gallery::app::AppState,
    actor: &gallery::auth::Actor,
    migration_id: &str,
    stage: &str,
) -> Value {
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let view = gallery::migrate_storage::get(&state.pool, actor, migration_id)
            .await
            .unwrap();
        if view["stage"] == stage {
            return view;
        }
    }
    panic!("migration did not reach {stage}");
}

#[tokio::test]
async fn an_old_route_epoch_cannot_publish() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::storage::bootstrap_local(&pool, dir.path().join("media"))
        .await
        .unwrap();
    let (backend, epoch) = gallery::db::current_route(&pool).await.unwrap();
    assert!(gallery::db::publish_route_ok(&pool, &backend, epoch).await.unwrap());
    sqlx::query("UPDATE storage_route SET routing_epoch = routing_epoch + 1 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!gallery::db::publish_route_ok(&pool, &backend, epoch).await.unwrap());
    let (still, next) = gallery::db::current_route(&pool).await.unwrap();
    assert_eq!(still, backend);
    assert_eq!(next, epoch + 1);
    assert!(gallery::db::publish_route_ok(&pool, &backend, next).await.unwrap());
}

#[tokio::test]
async fn a_download_keeps_the_route_it_opened() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Harness::new(dir.path().to_path_buf()).await;
    owner.login().await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (backend, epoch) = gallery::db::current_route(&pool).await.unwrap();
    let media = gallery::storage::open_by_id(&pool, &backend).await.unwrap();
    let bytes = png_bytes(8, 8, false);
    let sha = gallery::ids::sha256_hex(&bytes);
    let key = format!("media/ab/img-pin/{sha}.png");
    media.put_bytes(&key, &bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &backend,
        &key,
    )
    .await
    .unwrap();
    let (seq,): (i64,) = sqlx::query_as("SELECT COALESCE(MAX(created_seq), 0) + 1 FROM images")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-pin', ?, ?, 'pin.png', 8, 8, 'image/png', ?, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(seq)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let response = owner
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/library/images/img-pin/original-download")
                .header(header::COOKIE, format!("gallery_session={}", owner.cookie))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let pinned: (String, i64, String) = sqlx::query_as(
        "SELECT backend_id, routing_epoch, blob_id FROM media_reads",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pinned.0, backend);
    assert_eq!(pinned.1, epoch);
    assert_eq!(pinned.2, blob_id);
    sqlx::query("UPDATE storage_route SET routing_epoch = routing_epoch + 1 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let still: (i64,) = sqlx::query_as("SELECT routing_epoch FROM media_reads")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still.0, epoch);
    let downloaded = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(downloaded, bytes);
    let mut left = 1_i64;
    for _ in 0..50 {
        left = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM media_reads")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0;
        if left == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(left, 0);
}

#[tokio::test]
async fn an_open_read_blocks_retire_of_that_backend() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-pin', 'local', '{\"kind\":\"local\",\"root\":\"/tmp/gallery-unused\"}', 1, ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state,
            protected_until, created_at, updated_at
         ) VALUES('mig-pin', ?, 'target-pin', 'forward', 'protected', 'protected', ?, ?, ?)",
    )
    .bind(&source)
    .bind(now - 1_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    for id in ["bak-pin-1", "bak-pin-2"] {
        sqlx::query(
            "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at) VALUES(?, 'succeeded', ?, ?, ?)",
        )
        .bind(id)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO media_reads(holder, backend_id, routing_epoch, blob_id, until_ms)
         VALUES('expired-read', ?, 1, 'old-blob', ?)",
    )
    .bind(&source)
    .bind(now - 1_000)
    .execute(&state.pool)
    .await
    .unwrap();
    let pin = gallery::storage::pin_media_read(&state.pool, &source, 1, "live-blob")
        .await
        .unwrap();
    let blocked = gallery::migrate_storage::retire(&state, &actor, "mig-pin", true, None, None)
        .await
        .unwrap();
    assert_eq!(blocked["active_reads"], 1);
    assert!(blocked["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item == "active reads"));
    drop(pin);
    let mut open = true;
    for _ in 0..50 {
        let preview = gallery::migrate_storage::retire(&state, &actor, "mig-pin", true, None, None)
            .await
            .unwrap();
        open = preview["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "active reads");
        if !open && preview["active_reads"] == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!open);
}

#[tokio::test]
async fn remote_copy_streams_ranges_onto_the_target_file() {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static STORE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    let payload = b"range-copied-object";
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().to_string();
        if *req.method() == axum::http::Method::GET {
            let Some(bytes) = store.lock().unwrap().get(&path).cloned() else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let span = req
                .headers()
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("bytes="))
                .and_then(|value| value.split_once('-'))
                .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)));
            if let Some((start, end)) = span {
                return (StatusCode::PARTIAL_CONTENT, bytes[start..=end].to_vec()).into_response();
            }
            return (StatusCode::OK, bytes).into_response();
        }
        if *req.method() == axum::http::Method::PUT {
            let bytes = req.into_body().collect().await.unwrap().to_bytes().to_vec();
            store.lock().unwrap().insert(path, bytes);
            return StatusCode::OK.into_response();
        }
        if *req.method() == axum::http::Method::HEAD {
            return if store.lock().unwrap().contains_key(&path) {
                StatusCode::OK.into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            };
        }
        StatusCode::OK.into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let source = gallery::storage::s3::S3Backend::new(
        Some(format!("http://{addr}")),
        "us-east-1".into(),
        "gallery".into(),
        "".into(),
        Some("test-key".into()),
        Some("test-secret".into()),
    );
    gallery::storage::MediaBackend::put_immutable(&source, "media/a.bin", payload, "application/octet-stream")
        .await
        .unwrap();
    let source = gallery::storage::Storage::new("src".into(), std::sync::Arc::new(source));
    let dir = tempfile::tempdir().unwrap();
    let target = gallery::storage::Storage::new(
        "dst".into(),
        std::sync::Arc::new(gallery::storage::local::LocalFs::new(dir.path().to_path_buf())),
    );
    let sha = gallery::ids::sha256_hex(payload);
    gallery::migrate_storage::stream_object_copy(
        &source,
        &target,
        "media/a.bin",
        payload.len() as i64,
        &sha,
        "application/octet-stream",
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(dir.path().join("media/a.bin")).unwrap(), payload);
}

#[tokio::test]
async fn copy_streams_from_the_recorded_source_and_publishes_after_the_hash() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let source = state.media().await.unwrap();
    let bytes = vec![9u8; 200_000];
    let sha = gallery::ids::sha256_hex(&bytes);
    let key = format!("media/ab/img-copy/{sha}.bin");
    source.put_bytes(&key, &bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &source.backend_id,
        &key,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let target_root = dir.path().join("target-media");
    let target_cfg = gallery::storage::BackendConfig::Local { root: target_root.clone() };
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-copy', 'local', ?, 1, ?)",
    )
    .bind(serde_json::to_string(&target_cfg).unwrap())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-copy', ?, 'target-copy', 'forward', 'copying', 'running', ?, ?)",
    )
    .bind(&source.backend_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
         VALUES('mig-copy', ?, ?, ?, 'pending')",
    )
    .bind(&blob_id)
    .bind(&sha)
    .bind(bytes.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET active_backend_id = 'target-copy' WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let payload = serde_json::json!({
        "scope": {"migration_id": "mig-copy", "blob_id": blob_id},
        "target": {"target_backend_id": "target-copy"}
    })
    .to_string();
    gallery::migrate_storage::copy_item(&state, &payload).await.unwrap();
    let stable = target_root.join(&key);
    assert_eq!(std::fs::read(&stable).unwrap(), bytes);
    assert!(std::fs::read_dir(target_root.join("work/migrate/mig-copy"))
        .map(|entries| {
            entries.flatten().all(|entry| {
                !entry.file_name().to_string_lossy().ends_with(".partial")
            })
        })
        .unwrap_or(true));
    let located: (String,) = sqlx::query_as(
        "SELECT state FROM media_locations WHERE blob_id = ? AND backend_id = 'target-copy'",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(located.0, "verified");
    let active: (String,) = sqlx::query_as("SELECT active_backend_id FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(active.0, "target-copy");

    let bad = b"short-and-different";
    let bad_sha = gallery::ids::sha256_hex(bad);
    let bad_key = format!("media/ab/img-bad/{bad_sha}.bin");
    let source_again = gallery::storage::open_by_id(&state.pool, &source.backend_id).await.unwrap();
    source_again.put_bytes(&bad_key, bad, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let bad_blob = gallery::storage::record_blob(
        &mut tx,
        &bad_sha,
        bad.len() as i64,
        "application/octet-stream",
        "original",
        &source.backend_id,
        &bad_key,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let source_file = dir.path().join("media").join(&bad_key);
    std::fs::write(&source_file, b"changed").unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
         VALUES('mig-copy', ?, ?, ?, 'pending')",
    )
    .bind(&bad_blob)
    .bind(&bad_sha)
    .bind(bad.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    let bad_payload = serde_json::json!({
        "scope": {"migration_id": "mig-copy", "blob_id": bad_blob},
        "target": {"target_backend_id": "target-copy"}
    })
    .to_string();
    let err = gallery::migrate_storage::copy_item(&state, &bad_payload).await.unwrap_err();
    assert!(err.to_string().contains("source object mismatch"));
    assert!(!target_root.join(&bad_key).exists());
    assert!(std::fs::read_dir(target_root.join("work/migrate/mig-copy"))
        .map(|entries| {
            entries.flatten().all(|entry| {
                !entry.file_name().to_string_lossy().ends_with(".partial")
            })
        })
        .unwrap_or(true));
    let published: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM media_locations WHERE blob_id = ? AND backend_id = 'target-copy'",
    )
    .bind(&bad_blob)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(published.0, 0);
}

#[tokio::test]
async fn a_complete_target_is_adopted_and_drift_returns_to_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap();
    let bytes = b"already-on-the-target";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-adopt/{sha}.bin");
    source.put_bytes(&key, bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &source.backend_id,
        &key,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let source_file = dir.path().join("media").join(&key);
    std::fs::remove_file(&source_file).unwrap();
    let target_root = dir.path().join("target-adopt");
    let stable = target_root.join(&key);
    std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
    std::fs::write(&stable, bytes).unwrap();
    let leftover = target_root.join(format!("work/migrate/mig-adopt/{blob_id}-7.partial"));
    std::fs::create_dir_all(leftover.parent().unwrap()).unwrap();
    std::fs::write(&leftover, b"old-attempt").unwrap();
    let now = gallery::db::now_ms();
    let target_cfg = gallery::storage::BackendConfig::Local { root: target_root.clone() };
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-adopt', 'local', ?, 1, ?)",
    )
    .bind(serde_json::to_string(&target_cfg).unwrap())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-adopt', ?, 'target-adopt', 'forward', 'copying', 'paused', ?, ?)",
    )
    .bind(&source.backend_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
         VALUES('mig-adopt', ?, ?, ?, 'pending')",
    )
    .bind(&blob_id)
    .bind(&sha)
    .bind(bytes.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    let payload = serde_json::json!({
        "scope": {"migration_id": "mig-adopt", "blob_id": blob_id},
        "target": {"target_backend_id": "target-adopt"}
    })
    .to_string();
    gallery::migrate_storage::copy_item(&state, &payload).await.unwrap();
    assert!(!source_file.exists());
    assert!(!leftover.exists());
    let item: (String, i64) = sqlx::query_as(
        "SELECT state, attempt_no FROM storage_migration_items WHERE migration_id = 'mig-adopt' AND blob_id = ?",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(item.0, "verified");
    assert_eq!(item.1, 0);
    std::fs::write(&stable, b"drifted").unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, max_attempts, available_at, created_at, updated_at)
         VALUES('job-adopt', 'storage_migrate_copy', ?, ?, '{}', 'succeeded', 3, ?, ?, ?)",
    )
    .bind(format!("storage_migrate_copy:mig-adopt:{blob_id}"))
    .bind(&payload)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    gallery::migrate_storage::resume(&state, &actor, "mig-adopt").await.unwrap();
    let again: (String,) = sqlx::query_as(
        "SELECT state FROM storage_migration_items WHERE migration_id = 'mig-adopt' AND blob_id = ?",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(again.0, "pending");
    let locations: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM media_locations WHERE blob_id = ? AND backend_id = 'target-adopt'",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(locations.0, 0);
    let job: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-adopt'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(job.0, "queued");
}

#[tokio::test]
async fn resume_stops_when_the_endpoint_revision_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap().backend_id;
    let (source_rev,): (i64,) =
        sqlx::query_as("SELECT config_revision FROM storage_backends WHERE id = ?")
            .bind(&source)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-rev', 'local', '{\"kind\":\"local\",\"root\":\"/tmp/gallery-rev\"}', 1, ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state,
            source_config_revision, target_config_revision, created_at, updated_at
         ) VALUES('mig-rev', ?, 'target-rev', 'forward', 'copying', 'paused', ?, 1, ?, ?)",
    )
    .bind(&source)
    .bind(source_rev)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-rev', 'storage_migrate_copy', 'storage_migrate_copy:mig-rev:blob', '{}', '{}', 'retry_wait', ?, ?, ?)",
    )
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_backends SET config_revision = config_revision + 1 WHERE id = 'target-rev'")
        .execute(&state.pool)
        .await
        .unwrap();
    let err = gallery::migrate_storage::resume(&state, &actor, "mig-rev")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("CONFIG_CHANGED"));
    let job: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-rev'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(job.0, "retry_wait");
    let stage: (String,) = sqlx::query_as("SELECT state FROM storage_migrations WHERE id = 'mig-rev'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stage.0, "paused");
}

#[tokio::test]
async fn protection_window_keeps_the_last_backup_from_before_cutover() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media"))
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    let day = 24 * 3600 * 1000;
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-keep', 'local', '{\"kind\":\"local\",\"root\":\"/tmp/gallery-keep-backup\"}', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let until = now + day;
    let cutover = now - 40 * day;
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, protected_until, cutover_at, created_at, updated_at
         ) VALUES('mig-keep', ?, 'target-keep', 'forward', 'protected', 'protected', ?, ?, ?, ?)",
    )
    .bind(&storage.backend_id)
    .bind(until)
    .bind(cutover)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    for (id, at) in [
        ("bak-ancient", now - 80 * day),
        ("bak-pre", now - 50 * day),
        ("bak-d0", now),
        ("bak-d1", now - day),
        ("bak-d2", now - 2 * day),
        ("bak-d3", now - 8 * day),
        ("bak-d4", now - 16 * day),
        ("bak-d5", now - 24 * day),
        ("bak-d6", now - 32 * day),
    ] {
        sqlx::query(
            "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
             VALUES(?, 'succeeded', ?, ?, ?)",
        )
        .bind(id)
        .bind(at)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    let expired = gallery::backup::prune_retention(&pool, dir.path(), now).await.unwrap();
    assert!(expired.iter().any(|id| id == "bak-ancient"));
    assert!(!expired.iter().any(|id| id == "bak-pre"));
    let kept: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-pre'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept.0, "succeeded");
    let newest: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-d0'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(newest.0, "succeeded");
}

#[tokio::test]
async fn retire_waits_for_a_drill_and_an_idle_independent_backup() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-drill', 'local', '{\"kind\":\"local\",\"root\":\"/tmp/gallery-drill-target\"}', 1, ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state,
            snapshot_catalog_revision, protected_until, created_at, updated_at
         ) VALUES('mig-drill', ?, 'target-drill', 'forward', 'protected', 'protected', 1, ?, ?, ?)",
    )
    .bind(&source)
    .bind(now - 1_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, catalog_revision, created_at, updated_at)
         VALUES('bak-a', 'succeeded', ?, 2, ?, ?),
                ('bak-b', 'succeeded', ?, 2, ?, ?),
                ('bak-run', 'running', ?, 2, ?, ?),
                ('bak-bad', 'unavailable', ?, 2, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now + 1)
    .bind(now)
    .bind(now)
    .bind(now + 2)
    .bind(now)
    .bind(now)
    .bind(now + 3)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let blocked = gallery::migrate_storage::retire(&state, &actor, "mig-drill", true, None, None)
        .await
        .unwrap();
    let blockers = blocked["blockers"].as_array().unwrap();
    assert!(blockers.iter().any(|item| item == "restore drill"));
    assert!(blockers.iter().any(|item| item == "backup in progress"));
    assert!(blockers.iter().any(|item| item == "backup unavailable"));
    let refused = gallery::backup::select_restore_branch(&state.pool, "bak-bad", None).await;
    assert!(refused.unwrap_err().to_string().contains("BACKUP_UNAVAILABLE"));
    assert!(!blockers.iter().any(|item| item == "backup on retired backend"));
    let media = dir.path().join("media");
    let nested = media.join("backups");
    assert!(gallery::migrate_storage::repository_inside(&media, &nested));
    assert!(!gallery::migrate_storage::repository_inside(&media, &dir.path().join("backups")));
}

#[tokio::test]
async fn retire_requires_protection_window_and_two_backups() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let media = state.media().await.unwrap();
    let original = media.backend_id.clone();
    let bytes = b"retire-source-bytes";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-retire/{sha}.bin");
    media.put_bytes(&key, bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &original,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-retire', ?, ?, 'a.png', 8, 8, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    let pinned_bytes = b"pinned-by-backup";
    let pinned_sha = gallery::ids::sha256_hex(pinned_bytes);
    let pinned_key = format!("media/ab/img-pinned/{pinned_sha}.bin");
    media
        .put_bytes(&pinned_key, pinned_bytes, "image/png")
        .await
        .unwrap();
    let pinned_blob = gallery::storage::record_blob(
        &mut tx,
        &pinned_sha,
        pinned_bytes.len() as i64,
        "image/png",
        "original",
        &original,
        &pinned_key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-pinned', ?, ?, 'pinned.png', 8, 8, 'image/png', 2, ?)",
    )
    .bind(&pinned_sha)
    .bind(&pinned_blob)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let target = dir.path().join("retire-media");
    let started = gallery::migrate_storage::start(
        &state,
        &actor,
        gallery::storage::BackendConfig::Local { root: target.clone() },
    )
    .await
    .unwrap();
    let migration_id = started["id"].as_str().unwrap();
    let ready = wait_migration_stage(&state, &actor, migration_id, "ready").await;
    gallery::migrate_storage::cutover(&state, &actor, migration_id, ready["revision"].as_i64().unwrap())
        .await
        .unwrap();
    let source_file = dir.path().join("media").join(&key);
    assert!(source_file.is_file());
    sqlx::query("UPDATE storage_migrations SET protected_until = ? WHERE id = ?")
        .bind(gallery::db::now_ms() - 1_000)
        .bind(migration_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let missing_backups = gallery::migrate_storage::retire(&state, &actor, migration_id, true, None, None)
        .await
        .unwrap();
    assert!(missing_backups["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item == "post-cutover backups"));
    assert!(!missing_backups["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item == "protection period"));
    let now = gallery::db::now_ms();
    let (floor,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(snapshot_catalog_revision, 0) FROM storage_migrations WHERE id = ?",
    )
    .bind(migration_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, catalog_revision, created_at, updated_at)
         VALUES('bak-1', 'succeeded', ?, ?, ?, ?),
                ('bak-2', 'succeeded', ?, ?, ?, ?),
                ('bak-finished', 'succeeded', ?, ?, ?, ?)",
    )
    .bind(now)
    .bind(floor + 1)
    .bind(now)
    .bind(now)
    .bind(now + 1)
    .bind(floor + 1)
    .bind(now)
    .bind(now)
    .bind(now + 10)
    .bind(floor)
    .bind(now)
    .bind(now + 10_000)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO restore_runs(id, backup_id, state, created_at, updated_at)
         VALUES('restore-drill', 'bak-2', 'succeeded', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES('bak-1', ?)")
        .bind(&pinned_blob)
        .execute(&state.pool)
        .await
        .unwrap();
    let preview = gallery::migrate_storage::retire(&state, &actor, migration_id, true, None, None)
        .await
        .unwrap();
    assert!(preview["blockers"].as_array().unwrap().is_empty());
    assert_eq!(preview["drill_backup_id"], "bak-2");
    assert!(preview["pre_migration_backup_id"].is_null());
    assert_eq!(preview["object_count"], 2);
    assert_eq!(preview["backup_exceptions"][0], pinned_blob);
    assert!(!preview.to_string().contains("secret"));
    let stale = gallery::migrate_storage::retire(
        &state,
        &actor,
        migration_id,
        false,
        Some(preview["revision"].as_i64().unwrap() + 9),
        preview["preview_sha"].as_str(),
    )
    .await;
    assert!(stale.unwrap_err().to_string().contains("REVISION_CONFLICT"));
    let retired = gallery::migrate_storage::retire(
        &state,
        &actor,
        migration_id,
        false,
        preview["revision"].as_i64(),
        preview["preview_sha"].as_str(),
    )
    .await
    .unwrap();
    assert_eq!(retired["stage"], "retired");
    assert!(!source_file.is_file());
    assert!(dir.path().join("media").join(&pinned_key).is_file());
    let progress: (i64, i64, i64) = sqlx::query_as(
        "SELECT retire_done, retire_total, retire_errors FROM storage_migrations WHERE id = ?",
    )
    .bind(migration_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(progress, (1, 1, 0));
    let config: (String,) = sqlx::query_as("SELECT config_json FROM storage_backends WHERE id = ?")
        .bind(&original)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&config.0).unwrap();
    assert_eq!(parsed["root"], "");
    assert!(!config.0.contains("secret_key"));
    assert!(!config.0.contains("password"));
    assert_eq!(state.media().await.unwrap().get_bytes(&key).await.unwrap(), bytes);
    let rolled = gallery::migrate_storage::rollback(&state, &actor, migration_id).await;
    assert!(rolled.unwrap_err().to_string().contains("RETIRE_BLOCKED"));
}

#[tokio::test]
async fn migration_plan_waits_for_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let media = state.media().await.unwrap();
    let bytes = b"plan-before-copy";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/plan/{sha}.bin");
    media.put_bytes(&key, bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &media.backend_id,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-plan', ?, ?, 'a.png', 8, 8, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let planned = gallery::migrate_storage::plan(
        &state,
        &actor,
        gallery::storage::BackendConfig::Local {
            root: dir.path().join("planned-media"),
        },
    )
    .await
    .unwrap();
    assert_eq!(planned["stage"], "planned");
    assert_eq!(planned["object_count"], 1);
    assert_eq!(planned["byte_count"], bytes.len() as i64);
    assert_eq!(planned["next_action"], "start");
    assert!(!planned.to_string().contains("secret"));
    let barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(barrier.0, 0);
    let jobs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE kind = 'storage_migrate_copy'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(jobs.0, 0);
    let migration_id = planned["id"].as_str().unwrap();
    let stale = gallery::migrate_storage::begin(&state, &actor, migration_id, 99).await;
    assert!(stale.unwrap_err().to_string().contains("REVISION_CONFLICT"));
    sqlx::query("UPDATE storage_route SET catalog_revision = catalog_revision + 1 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let drifted = gallery::migrate_storage::begin(
        &state,
        &actor,
        migration_id,
        planned["revision"].as_i64().unwrap(),
    )
    .await;
    assert!(drifted.unwrap_err().to_string().contains("ROUTE_CHANGED"));
    sqlx::query("UPDATE storage_route SET catalog_revision = ? WHERE id = 1")
        .bind(planned["catalog_revision"].as_i64().unwrap())
        .execute(&state.pool)
        .await
        .unwrap();
    let started = gallery::migrate_storage::begin(
        &state,
        &actor,
        migration_id,
        planned["revision"].as_i64().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(started["write_barrier"], true);
    assert_eq!(started["job_ids"].as_array().unwrap().len(), 1);
    let jobs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE kind = 'storage_migrate_copy'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(jobs.0, 1);
}

#[tokio::test]
async fn migration_pause_and_abort_leave_route() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap().backend_id;
    let target = gallery::ids::new_id();
    let migration = gallery::ids::new_id();
    let now = gallery::db::now_ms();
    sqlx::query("INSERT INTO storage_backends(id, kind, config_json, created_at) VALUES(?, 'local', ?, ?)")
        .bind(&target)
        .bind(serde_json::json!({"kind":"local","root": dir.path().join("other").to_string_lossy()}).to_string())
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at)
         VALUES(?, ?, ?, 'forward', 'copying', 'running', ?, ?)",
    )
    .bind(&migration)
    .bind(&source)
    .bind(&target)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET write_barrier = 1, current_migration_id = ? WHERE id = 1")
        .bind(&migration)
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-copy', 'storage_migrate_copy', ?, '{}', '{}', 'queued', ?, ?, ?)",
    )
    .bind(format!("storage_migrate_copy:{migration}:blob"))
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let paused = gallery::migrate_storage::pause(&state, &actor, &migration).await.unwrap();
    assert_eq!(paused["state"], "paused");
    let job = gallery::jobs::get(&state.pool, "job-copy").await.unwrap().unwrap();
    assert_eq!(job.state, "retry_wait");
    let aborted = gallery::migrate_storage::abort(&state, &actor, &migration).await.unwrap();
    assert_eq!(aborted["stage"], "aborted");
    let job = gallery::jobs::get(&state.pool, "job-copy").await.unwrap().unwrap();
    assert_eq!(job.state, "canceled");
    let barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(barrier.0, 0);
    assert_eq!(state.media().await.unwrap().backend_id, source);
}

#[tokio::test]
async fn abort_cleans_unreferenced_target_copies_after_the_source_answers() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap();
    let bytes = b"copied-but-not-cut-over";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-abort/{sha}.bin");
    source.put_bytes(&key, bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &source.backend_id,
        &key,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let target_root = dir.path().join("abort-target");
    let copied = target_root.join(&key);
    std::fs::create_dir_all(copied.parent().unwrap()).unwrap();
    std::fs::write(&copied, bytes).unwrap();
    let partial = target_root.join(format!("work/migrate/mig-abort/{blob_id}-1.partial"));
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, b"partial").unwrap();
    let now = gallery::db::now_ms();
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"file").unwrap();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('source-blocked', 'local', ?, 1, ?)",
    )
    .bind(serde_json::json!({"kind":"local","root": blocker}).to_string())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let target_cfg = gallery::storage::BackendConfig::Local { root: target_root.clone() };
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-abort', 'local', ?, 1, ?)",
    )
    .bind(serde_json::to_string(&target_cfg).unwrap())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-abort', 'source-blocked', 'target-abort', 'forward', 'copying', 'running', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
         VALUES('mig-abort', ?, ?, ?, 'verified')",
    )
    .bind(&blob_id)
    .bind(&sha)
    .bind(bytes.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, size, sha256, state, verified_at)
         VALUES(?, 'target-abort', ?, ?, ?, 'verified', ?)",
    )
    .bind(&blob_id)
    .bind(&key)
    .bind(bytes.len() as i64)
    .bind(&sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET write_barrier = 1, current_migration_id = 'mig-abort' WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let blocked = gallery::migrate_storage::abort(&state, &actor, "mig-abort")
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("SOURCE_UNREADABLE"));
    assert!(copied.is_file());
    assert!(partial.is_file());
    let barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(barrier.0, 1);
    sqlx::query("UPDATE storage_migrations SET source_backend_id = ? WHERE id = 'mig-abort'")
        .bind(&source.backend_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let aborted = gallery::migrate_storage::abort(&state, &actor, "mig-abort")
        .await
        .unwrap();
    assert_eq!(aborted["stage"], "aborted");
    assert!(aborted["cleaned"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item == &key));
    assert!(aborted["cleaned"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item.as_str().unwrap_or("").ends_with(".partial")));
    assert!(aborted["cleanup_errors"].as_array().unwrap().is_empty());
    assert!(!copied.exists());
    assert!(!partial.exists());
    assert!(dir.path().join("media").join(&key).is_file());
    let left: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM media_locations WHERE blob_id = ? AND backend_id = 'target-abort'",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(left.0, 0);
    let audit: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'migration.abort' AND target = 'mig-abort'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(audit.0, 1);
    let open: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(open.0, 0);
}

#[tokio::test]
async fn opening_keeps_the_write_barrier_until_the_smoke_passes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap();
    let bytes = b"smoke-this-original";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-open/{sha}.bin");
    source.put_bytes(&key, bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &source.backend_id,
        &key,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (catalog,): (i64,) = sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let target_root = dir.path().join("open-target");
    let now = gallery::db::now_ms();
    let target_cfg = gallery::storage::BackendConfig::Local { root: target_root.clone() };
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-open', 'local', ?, 1, ?)",
    )
    .bind(serde_json::to_string(&target_cfg).unwrap())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state,
            snapshot_catalog_revision, created_at, updated_at
         ) VALUES('mig-open', ?, 'target-open', 'forward', 'copying', 'running', ?, ?, ?)",
    )
    .bind(&source.backend_id)
    .bind(catalog)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
         VALUES('mig-open', ?, ?, ?, 'pending')",
    )
    .bind(&blob_id)
    .bind(&sha)
    .bind(bytes.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    let payload = serde_json::json!({
        "scope": {"migration_id": "mig-open", "blob_id": blob_id},
        "target": {"target_backend_id": "target-open"}
    })
    .to_string();
    gallery::migrate_storage::copy_item(&state, &payload).await.unwrap();
    let (revision,): (i64,) = sqlx::query_as("SELECT revision FROM storage_migrations WHERE id = 'mig-open'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let opening_revision = revision + 1;
    sqlx::query(
        "UPDATE storage_migrations SET stage = 'opening', state = 'opening', revision = ? WHERE id = 'mig-open'",
    )
    .bind(opening_revision)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE storage_route SET active_backend_id = 'target-open', routing_epoch = routing_epoch + 1,
            write_barrier = 0, current_migration_id = NULL WHERE id = 1",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    gallery::migrate_storage::recover_opening(&state.pool).await.unwrap();
    let held: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT active_backend_id, write_barrier, current_migration_id FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(held.0, "target-open");
    assert_eq!(held.1, 1);
    assert_eq!(held.2.as_deref(), Some("mig-open"));
    let target_file = target_root.join(&key);
    std::fs::write(&target_file, b"broken").unwrap();
    let failed = gallery::migrate_storage::cutover(&state, &actor, "mig-open", opening_revision)
        .await
        .unwrap_err();
    assert!(failed.to_string().contains("CHECKSUM_MISMATCH"));
    let stuck: (String, i64) = sqlx::query_as(
        "SELECT stage, (SELECT write_barrier FROM storage_route WHERE id = 1) FROM storage_migrations WHERE id = 'mig-open'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(stuck.0, "opening");
    assert_eq!(stuck.1, 1);
    std::fs::write(&target_file, bytes).unwrap();
    let opened = gallery::migrate_storage::cutover(&state, &actor, "mig-open", opening_revision)
        .await
        .unwrap();
    assert_eq!(opened["stage"], "protected");
    assert_eq!(opened["write_barrier"], false);
    assert!(!target_root.join("work/migrate-smoke/mig-open.bin").exists());
    let catalog_after: (i64,) = sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(catalog_after.0, catalog);
    assert!(dir.path().join("media").join(&key).is_file());
}

#[tokio::test]
async fn rollback_before_open_returns_to_the_source_without_reading_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap();
    let bytes = b"still-on-the-source";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-back/{sha}.bin");
    source.put_bytes(&key, bytes, "application/octet-stream").await.unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        &source.backend_id,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-back', ?, ?, 'back.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (catalog, epoch): (i64, i64) = sqlx::query_as(
        "SELECT catalog_revision, routing_epoch FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let target_root = dir.path().join("back-target");
    let target_cfg = gallery::storage::BackendConfig::Local { root: target_root.clone() };
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-back', 'local', ?, 1, ?)",
    )
    .bind(serde_json::to_string(&target_cfg).unwrap())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state,
            snapshot_catalog_revision, created_at, updated_at
         ) VALUES('mig-back', ?, 'target-back', 'forward', 'opening', 'opening', ?, ?, ?)",
    )
    .bind(&source.backend_id)
    .bind(catalog)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
         VALUES('mig-back', ?, ?, ?, 'verified')",
    )
    .bind(&blob_id)
    .bind(&sha)
    .bind(bytes.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    let target_file = target_root.join(&key);
    std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
    std::fs::write(&target_file, b"target-is-corrupt").unwrap();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, size, sha256, state, verified_at)
         VALUES(?, 'target-back', ?, ?, ?, 'verified', ?)",
    )
    .bind(&blob_id)
    .bind(&key)
    .bind(bytes.len() as i64)
    .bind(&sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE storage_route SET active_backend_id = 'target-back', write_barrier = 1, current_migration_id = 'mig-back' WHERE id = 1",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let rolled = gallery::migrate_storage::rollback(&state, &actor, "mig-back")
        .await
        .unwrap();
    assert_eq!(rolled["stage"], "protected");
    assert!(rolled["protected_until"].as_i64().unwrap() > gallery::db::now_ms());
    assert_eq!(rolled["active_backend_id"], source.backend_id);
    assert_eq!(rolled["write_barrier"], false);
    assert_eq!(rolled["routing_epoch"], epoch + 1);
    let again = gallery::migrate_storage::rollback(&state, &actor, "mig-back")
        .await
        .unwrap_err();
    assert!(again.to_string().contains("MIGRATION_STAGE"));
    assert_eq!(std::fs::read(dir.path().join("media").join(&key)).unwrap(), bytes);
    assert_eq!(std::fs::read(&target_file).unwrap(), b"target-is-corrupt");
    assert!(!dir.path().join("media").join("work/migrate-smoke/mig-back.bin").exists());
}

#[tokio::test]
async fn unreadable_backend_rolls_back_from_the_source_or_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap();
    let kept = b"already-on-source";
    let kept_sha = gallery::ids::sha256_hex(kept);
    let kept_key = format!("media/ab/img-kept/{kept_sha}.bin");
    source.put_bytes(&kept_key, kept, "application/octet-stream").await.unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let kept_blob = gallery::storage::record_blob(
        &mut tx,
        &kept_sha,
        kept.len() as i64,
        "application/octet-stream",
        "original",
        &source.backend_id,
        &kept_key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-kept', ?, ?, 'kept.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&kept_sha)
    .bind(&kept_blob)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let only = b"only-in-backup";
    let only_sha = gallery::ids::sha256_hex(only);
    let only_key = format!("media/ab/img-only/{only_sha}.bin");
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES('blob-only', ?, ?, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(&only_sha)
    .bind(only.len() as i64)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let blocked_root = dir.path().join("not-a-directory");
    std::fs::write(&blocked_root, b"file").unwrap();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-dark', 'local', ?, 1, ?)",
    )
    .bind(serde_json::json!({"kind":"local","root": blocked_root}).to_string())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, size, sha256, state, verified_at)
         VALUES('blob-only', 'target-dark', ?, ?, ?, 'verified', ?)",
    )
    .bind(&only_key)
    .bind(only.len() as i64)
    .bind(&only_sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, size, sha256, state, verified_at)
         VALUES(?, 'target-dark', ?, ?, ?, 'verified', ?)",
    )
    .bind(&kept_blob)
    .bind(&kept_key)
    .bind(kept.len() as i64)
    .bind(&kept_sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-only', ?, 'blob-only', 'only.bin', 8, 8, 'application/octet-stream', 2, ?)",
    )
    .bind(&only_sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-dark', ?, 'target-dark', 'forward', 'protected', 'protected', ?, ?)",
    )
    .bind(&source.backend_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
         VALUES('mig-dark', ?, ?, ?, 'verified')",
    )
    .bind(&kept_blob)
    .bind(&kept_sha)
    .bind(kept.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE storage_route SET active_backend_id = 'target-dark', write_barrier = 0, current_migration_id = NULL WHERE id = 1",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let blocked = gallery::migrate_storage::rollback(&state, &actor, "mig-dark")
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("ROLLBACK_MISSING_MEDIA"));
    let held: (String, i64) = sqlx::query_as(
        "SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(held.0, "target-dark");
    assert_eq!(held.1, 1);
    let ciphertext = gallery::backup::age_encrypt_bytes(&state.config.keys_root(), only).unwrap();
    let ciphertext_sha = gallery::ids::sha256_hex(&ciphertext);
    let backup_dir = state.config.backup_root().join("bak-only");
    std::fs::create_dir_all(&backup_dir).unwrap();
    std::fs::write(
        backup_dir.join(gallery::ids::sha256_hex(b"blob-only")),
        &ciphertext,
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-only', 'succeeded', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_items(backup_id, blob_id, ciphertext_key, plaintext_sha256, ciphertext_sha256, size, state)
         VALUES('bak-only', 'blob-only', 'blob-only', ?, ?, ?, 'verified')",
    )
    .bind(&only_sha)
    .bind(&ciphertext_sha)
    .bind(only.len() as i64)
    .execute(&state.pool)
    .await
    .unwrap();
    let rolled = gallery::migrate_storage::rollback(&state, &actor, "mig-dark")
        .await
        .unwrap();
    assert_eq!(rolled["stage"], "protected");
    assert_eq!(rolled["active_backend_id"], source.backend_id);
    assert_eq!(rolled["write_barrier"], false);
    assert_eq!(std::fs::read(dir.path().join("media").join(&kept_key)).unwrap(), kept);
    assert_eq!(std::fs::read(dir.path().join("media").join(&only_key)).unwrap(), only);
}

#[tokio::test]
async fn rollback_waits_for_in_flight_work_and_pins_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    let target_root = dir.path().join("rollback-target");
    std::fs::create_dir_all(&target_root).unwrap();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-wait', 'local', ?, 1, ?)",
    )
    .bind(serde_json::json!({"kind":"local","root": target_root}).to_string())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let sha = gallery::ids::sha256_hex(b"missing-on-both");
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES('blob-wait', ?, 16, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(&sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, size, sha256, state, verified_at)
         VALUES('blob-wait', 'target-wait', 'media/ab/wait.bin', 16, ?, 'verified', ?)",
    )
    .bind(&sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-wait', ?, 'blob-wait', 'wait.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&sha)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-wait', ?, 'target-wait', 'forward', 'protected', 'protected', ?, ?)",
    )
    .bind(&source)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE storage_route SET active_backend_id = 'target-wait' WHERE id = 1",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-inflight', 'generate_variant', 'dedupe-inflight', '{}', '{}', 'running', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let waiting = gallery::migrate_storage::rollback(&state, &actor, "mig-wait")
        .await
        .unwrap_err();
    assert!(waiting.to_string().contains("IN_FLIGHT"));
    let pinned: (Option<i64>,) = sqlx::query_as(
        "SELECT rollback_catalog_revision FROM storage_migrations WHERE id = 'mig-wait'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(pinned.0.is_none());
    let barrier: (i64,) = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(barrier.0, 1);
    sqlx::query("UPDATE jobs SET state = 'succeeded' WHERE id = 'job-inflight'")
        .execute(&state.pool)
        .await
        .unwrap();
    let missing = gallery::migrate_storage::rollback(&state, &actor, "mig-wait")
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("ROLLBACK_MISSING_MEDIA"));
    let (catalog, stored): (i64, i64) = sqlx::query_as(
        "SELECT r.catalog_revision, m.rollback_catalog_revision
         FROM storage_route r, storage_migrations m WHERE r.id = 1 AND m.id = 'mig-wait'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(stored, catalog);
    sqlx::query("UPDATE storage_route SET catalog_revision = catalog_revision + 1 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let drifted = gallery::migrate_storage::rollback(&state, &actor, "mig-wait")
        .await
        .unwrap_err();
    assert!(drifted.to_string().contains("ROUTE_CHANGED"));
    let route: (String, i64) = sqlx::query_as(
        "SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(route.0, "target-wait");
    assert_eq!(route.1, 1);
}

#[tokio::test]
async fn an_interrupted_rollback_resumes_verified_items() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source_id = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    let target_root = dir.path().join("resume-target");
    let target = gallery::storage::open_backend(
        "target-resume".into(),
        gallery::storage::BackendConfig::Local { root: target_root.clone() },
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-resume', 'local', ?, 1, ?)",
    )
    .bind(serde_json::json!({"kind":"local","root": target_root}).to_string())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let ready = b"copied-before-interrupt";
    let ready_sha = gallery::ids::sha256_hex(ready);
    let ready_key = format!("media/ab/img-ready/{ready_sha}.bin");
    target.put_bytes(&ready_key, ready, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let ready_blob = gallery::storage::record_blob(
        &mut tx,
        &ready_sha,
        ready.len() as i64,
        "application/octet-stream",
        "original",
        "target-resume",
        &ready_key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-ready', ?, ?, 'ready.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&ready_sha)
    .bind(&ready_blob)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    let later = b"copied-on-resume";
    let later_sha = gallery::ids::sha256_hex(later);
    let later_key = format!("media/ab/img-later/{later_sha}.bin");
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at)
         VALUES('blob-later', ?, ?, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(&later_sha)
    .bind(later.len() as i64)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, size, sha256, state, verified_at)
         VALUES('blob-later', 'target-resume', ?, ?, ?, 'verified', ?)",
    )
    .bind(&later_key)
    .bind(later.len() as i64)
    .bind(&later_sha)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-later', ?, 'blob-later', 'later.bin', 8, 8, 'application/octet-stream', 2, ?)",
    )
    .bind(&later_sha)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-resume', ?, 'target-resume', 'forward', 'protected', 'protected', ?, ?)",
    )
    .bind(&source_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET active_backend_id = 'target-resume' WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let blocked = gallery::migrate_storage::rollback(&state, &actor, "mig-resume")
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("ROLLBACK_MISSING_MEDIA"));
    let saved: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT state, attempt_no, lease_token FROM storage_rollback_items
         WHERE migration_id = 'mig-resume' AND blob_id = ?",
    )
    .bind(&ready_blob)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(saved.0, "verified");
    assert_eq!(saved.1, 1);
    assert!(saved.2.is_none());
    let waiting: (String, Option<i64>) = sqlx::query_as(
        "SELECT state, lease_until FROM storage_rollback_items
         WHERE migration_id = 'mig-resume' AND blob_id = 'blob-later'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(waiting.0, "pending");
    assert!(waiting.1.is_none());
    std::fs::write(target_root.join(&ready_key), b"corrupt").unwrap();
    target.put_bytes(&later_key, later, "application/octet-stream").await.unwrap();
    let rolled = gallery::migrate_storage::rollback(&state, &actor, "mig-resume")
        .await
        .unwrap();
    assert_eq!(rolled["stage"], "protected");
    assert_eq!(rolled["active_backend_id"], source_id);
    assert_eq!(std::fs::read(dir.path().join("media").join(&ready_key)).unwrap(), ready);
    assert_eq!(std::fs::read(dir.path().join("media").join(&later_key)).unwrap(), later);
    assert_eq!(std::fs::read(target_root.join(&ready_key)).unwrap(), b"corrupt");
}

#[tokio::test]
async fn a_live_rollback_lease_blocks_the_route_switch() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source_id = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    let target_root = dir.path().join("lease-target");
    let target = gallery::storage::open_backend(
        "target-lease".into(),
        gallery::storage::BackendConfig::Local { root: target_root.clone() },
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-lease', 'local', ?, 1, ?)",
    )
    .bind(serde_json::json!({"kind":"local","root": target_root}).to_string())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let bytes = b"held-by-another-attempt";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-lease/{sha}.bin");
    target.put_bytes(&key, bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        "target-lease",
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-lease', ?, ?, 'lease.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-lease', ?, 'target-lease', 'forward', 'protected', 'protected', ?, ?)",
    )
    .bind(&source_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_rollback_items(
            migration_id, blob_id, expected_sha256, expected_size, state, attempt_no, lease_token, lease_until
         ) VALUES('mig-lease', ?, ?, ?, 'copying', 1, 'lease-1', ?)",
    )
    .bind(&blob_id)
    .bind(&sha)
    .bind(bytes.len() as i64)
    .bind(now + 120_000)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET active_backend_id = 'target-lease' WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let held = gallery::migrate_storage::rollback(&state, &actor, "mig-lease")
        .await
        .unwrap_err();
    assert!(held.to_string().contains("LEASE_HELD"));
    let route: (String, i64) = sqlx::query_as(
        "SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(route.0, "target-lease");
    assert_eq!(route.1, 1);
    assert!(!dir.path().join("media").join(&key).exists());
    sqlx::query(
        "UPDATE storage_rollback_items SET lease_until = ? WHERE migration_id = 'mig-lease' AND blob_id = ?",
    )
    .bind(now - 1_000)
    .bind(&blob_id)
    .execute(&state.pool)
    .await
    .unwrap();
    let rolled = gallery::migrate_storage::rollback(&state, &actor, "mig-lease")
        .await
        .unwrap();
    assert_eq!(rolled["stage"], "protected");
    assert_eq!(rolled["active_backend_id"], source_id);
    assert_eq!(std::fs::read(dir.path().join("media").join(&key)).unwrap(), bytes);
    let attempt: (String, i64) = sqlx::query_as(
        "SELECT state, attempt_no FROM storage_rollback_items WHERE migration_id = 'mig-lease' AND blob_id = ?",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(attempt.0, "verified");
    assert_eq!(attempt.1, 2);
}

#[tokio::test]
async fn rollback_stops_after_three_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source_id = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    let target_root = dir.path().join("attempt-target");
    let target = gallery::storage::open_backend(
        "target-attempt".into(),
        gallery::storage::BackendConfig::Local { root: target_root.clone() },
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-attempt', 'local', ?, 1, ?)",
    )
    .bind(serde_json::json!({"kind":"local","root": target_root}).to_string())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let bytes = b"too-many-rollback-attempts";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-attempt/{sha}.bin");
    target.put_bytes(&key, bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        "target-attempt",
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-attempt', ?, ?, 'attempt.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-attempt', ?, 'target-attempt', 'forward', 'protected', 'protected', ?, ?)",
    )
    .bind(&source_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_rollback_items(
            migration_id, blob_id, expected_sha256, expected_size, state, attempt_no, lease_until
         ) VALUES('mig-attempt', ?, ?, ?, 'pending', 3, ?)",
    )
    .bind(&blob_id)
    .bind(&sha)
    .bind(bytes.len() as i64)
    .bind(now - 1_000)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET active_backend_id = 'target-attempt' WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let stopped = gallery::migrate_storage::rollback(&state, &actor, "mig-attempt")
        .await
        .unwrap_err();
    assert!(stopped.to_string().contains("ATTEMPTS_EXHAUSTED"));
    let item: (String, i64) = sqlx::query_as(
        "SELECT state, attempt_no FROM storage_rollback_items WHERE migration_id = 'mig-attempt'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(item.0, "failed");
    assert_eq!(item.1, 3);
    let route: (String, i64) = sqlx::query_as(
        "SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(route.0, "target-attempt");
    assert_eq!(route.1, 1);
    assert!(!dir.path().join("media").join(&key).exists());
}

#[tokio::test]
async fn stale_copy_partials_and_multipart_uploads_are_cleaned() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let root = dir.path().join("media");
    gallery::storage::bootstrap_local(&pool, root.clone()).await.unwrap();
    let old = root.join("work/migrate/mig-old/blob-1.partial");
    let fresh = root.join("work/migrate/mig-new/blob-2.partial");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
    std::fs::write(&old, b"old").unwrap();
    std::fs::write(&fresh, b"fresh").unwrap();
    let file = std::fs::File::options().write(true).open(&old).unwrap();
    file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600))
        .unwrap();
    let now = gallery::db::now_ms();
    let (backend,): (String,) = sqlx::query_as("SELECT id FROM storage_backends WHERE kind = 'local'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO storage_multipart_uploads(upload_id, backend_id, object_key, opened_at)
         VALUES('up-old', ?, 'media/ab/old.bin', ?), ('up-new', ?, 'media/ab/new.bin', ?)",
    )
    .bind(&backend)
    .bind(now - 48 * 3600 * 1000)
    .bind(&backend)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let (partials, uploads) = gallery::storage::reconcile::clean_attempt_leftovers(&pool)
        .await
        .unwrap();
    assert!(partials.iter().any(|key| key.ends_with("blob-1.partial")));
    assert!(!old.exists());
    assert!(fresh.exists());
    assert_eq!(uploads, 1);
    let left: (String,) = sqlx::query_as("SELECT upload_id FROM storage_multipart_uploads")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left.0, "up-new");
}

#[tokio::test]
async fn degrade_records_missing_originals_and_the_restore_point() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "workers_draining", "1").await.unwrap();
    tx.commit().await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source_id = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    let target_root = dir.path().join("degrade-target");
    let target = gallery::storage::open_backend(
        "target-degrade".into(),
        gallery::storage::BackendConfig::Local { root: target_root.clone() },
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-degrade', 'local', ?, 1, ?)",
    )
    .bind(serde_json::json!({"kind":"local","root": target_root}).to_string())
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let bytes = b"gone-from-both-ends";
    let sha = gallery::ids::sha256_hex(bytes);
    let key = format!("media/ab/img-gone/{sha}.bin");
    target.put_bytes(&key, bytes, "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "application/octet-stream",
        "original",
        "target-degrade",
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-gone', ?, ?, 'gone.bin', 8, 8, 'application/octet-stream', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-degrade', ?, 'target-degrade', 'forward', 'protected', 'protected', ?, ?)",
    )
    .bind(&source_id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-point', 'succeeded', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET active_backend_id = 'target-degrade' WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let wrong = gallery::migrate_storage::degrade(
        &state,
        &actor,
        "mig-degrade",
        &[blob_id.clone()],
        "bak-point",
    )
    .await
    .unwrap_err();
    assert!(wrong.to_string().contains("MISSING_SET"));
    std::fs::remove_file(target_root.join(&key)).unwrap();
    let bad_point = gallery::migrate_storage::degrade(
        &state,
        &actor,
        "mig-degrade",
        &[blob_id.clone()],
        "missing-backup",
    )
    .await
    .unwrap_err();
    assert!(bad_point.to_string().contains("RESTORE_POINT"));
    let degraded = gallery::migrate_storage::degrade(
        &state,
        &actor,
        "mig-degrade",
        &[blob_id.clone()],
        "bak-point",
    )
    .await
    .unwrap();
    assert_eq!(degraded["stage"], "protected");
    assert_eq!(degraded["active_backend_id"], source_id);
    assert_eq!(degraded["restore_point"], "bak-point");
    assert_eq!(degraded["exceptions"][0], blob_id);
    let item: (String,) = sqlx::query_as(
        "SELECT state FROM storage_rollback_items WHERE migration_id = 'mig-degrade' AND blob_id = ?",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(item.0, "exception");
    assert!(!dir.path().join("media").join(&key).exists());
    let audit: (String,) = sqlx::query_as(
        "SELECT detail FROM audit_log WHERE action = 'migration.degrade' AND target = 'mig-degrade'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(audit.0.contains("bak-point"));
    assert!(audit.0.contains(&blob_id));
}

#[tokio::test]
async fn migration_status_shows_the_reader_and_separate_byte_counts() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = state.media().await.unwrap().backend_id;
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-status', 'local', '{\"kind\":\"local\",\"root\":\"/tmp/gallery-status\"}', 1, ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at) VALUES
         ('blob-ok', 'aa', 10, 'application/octet-stream', 'verified', 'original', ?),
         ('blob-bad', 'bb', 7, 'application/octet-stream', 'verified', 'original', ?),
         ('blob-mid', 'cc', 4, 'application/octet-stream', 'verified', 'original', ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-status', ?, 'target-status', 'forward', 'copying', 'paused', ?, ?)",
    )
    .bind(&source)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migration_items(
            migration_id, blob_id, expected_sha256, expected_size, state, attempt_no, last_error
         ) VALUES
         ('mig-status', 'blob-ok', 'aa', 10, 'verified', 1, NULL),
         ('mig-status', 'blob-bad', 'bb', 7, 'failed', 2, 'checksum'),
         ('mig-status', 'blob-mid', 'cc', 4, 'copying', 3, NULL)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET write_barrier = 1, current_migration_id = 'mig-status' WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let view = gallery::migrate_storage::get(&state.pool, &actor, "mig-status")
        .await
        .unwrap();
    assert_eq!(view["reading"], "source");
    assert_eq!(view["bytes_verified"], 10);
    assert_eq!(view["bytes_failed"], 7);
    assert_eq!(view["bytes_unknown"], 4);
    assert_eq!(view["bytes_retried"], 11);
    assert_eq!(view["bytes_total"], 21);
    assert_eq!(view["failure_summary"]["errors"][0], "checksum");
    let paused = view["writes_paused"].as_array().unwrap();
    assert!(paused.iter().any(|item| item == "purge_media"));
    assert!(paused.iter().any(|item| item == "storage_migrate_copy"));
    assert!(!view.to_string().contains("secret"));
    gallery::migrate_storage::pause(&state, &actor, "mig-status")
        .await
        .unwrap();
    let event: (String,) = sqlx::query_as(
        "SELECT payload_json FROM events WHERE type = 'storage.migration.updated' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(event.0.contains("\"reading\":\"source\""));
    assert!(event.0.contains("\"bytes_verified\":10"));
    assert!(!event.0.contains("secret"));
    sqlx::query("UPDATE storage_route SET active_backend_id = 'target-status', write_barrier = 0 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE storage_migrations SET state = 'running' WHERE id = 'mig-status'")
        .execute(&state.pool)
        .await
        .unwrap();
    let live = gallery::migrate_storage::get(&state.pool, &actor, "mig-status")
        .await
        .unwrap();
    assert_eq!(live["reading"], "target");
    assert!(live["writes_paused"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn agent_stop_keeps_submitted_job() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0.clone(),
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at) VALUES('turn-1', ?, 1, 'running', ?)",
    )
    .bind(session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, actor_id, available_at, created_at, updated_at)
         VALUES('job-kept', 'reconcile', 'dedupe-kept', '{}', '{}', 'queued', ?, ?, ?, ?)",
    )
    .bind(&actor.id)
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turn_jobs(turn_id, job_id, submitted_at) VALUES('turn-1', 'job-kept', ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let (status, posted) = gallery::ai::post_message(
        &state.pool,
        &actor,
        session_id,
        gallery::ai::IncomingMessage {
            client_message_id: "m1".into(),
            text: "look at the album".into(),
            mode: Some("steer_if_active".into()),
            expected_turn_id: Some("turn-1".into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(status, 202);
    assert_eq!(posted["delivery_state"], "delivery_unknown");
    let (again, _) = gallery::ai::post_message(
        &state.pool,
        &actor,
        session_id,
        gallery::ai::IncomingMessage {
            client_message_id: "m1".into(),
            text: "look at the album".into(),
            mode: None,
            expected_turn_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(again, 200);
    let (queued_status, _) = gallery::ai::post_message(
        &state.pool,
        &actor,
        session_id,
        gallery::ai::IncomingMessage {
            client_message_id: "m2".into(),
            text: "next".into(),
            mode: Some("next_turn".into()),
            expected_turn_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(queued_status, 202);
    let stopped = gallery::ai::stop_turn(&state.pool, &actor, session_id, "turn-1")
        .await
        .unwrap();
    assert_eq!(stopped["state"], "stop_requested");
    let again = gallery::ai::stop_turn(&state.pool, &actor, session_id, "turn-1")
        .await
        .unwrap();
    assert_eq!(again["state"], "stop_requested");
    let job = gallery::jobs::get(&state.pool, "job-kept").await.unwrap().unwrap();
    assert_eq!(job.state, "queued");
    let paused: (String,) = sqlx::query_as(
        "SELECT delivery_state FROM agent_messages WHERE session_id = ? AND client_message_id = 'm2'",
    )
    .bind(session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(paused.0, "paused");
    let settled = gallery::ai::record_turn_terminal(
        &state.pool,
        &actor,
        session_id,
        "turn-1",
        "interrupted",
    )
    .await
    .unwrap();
    assert_eq!(settled["state"], "interrupted");
    let session_state: (String,) =
        sqlx::query_as("SELECT state FROM agent_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(session_state.0, "idle");
}

#[tokio::test]
async fn codec_worker_scales_png() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.png");
    std::fs::write(&input, png_bytes(100, 80, false)).unwrap();
    let plan = gallery::processing::CodecPlan {
        plan_id: "t".into(),
        ops: vec![gallery::processing::CodecOp {
            input_path: input,
            output_path: output.clone(),
            spec_id: "thumb".into(),
            format: "png".into(),
            width: Some(32),
            height: Some(32),
            long_edge: None,
            fit: "cover".into(),
        }],
    };
    let out = gallery::processing::run_plan(&plan).unwrap();
    assert_eq!(out[0].width, 32);
    assert_eq!(out[0].height, 32);
    assert!(output.exists());
}

#[tokio::test]
async fn agent_album_list_tool_does_not_call_a_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let created = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum {
            name: "Tools".into(),
            description: None,
        },
    )
    .await
    .unwrap();
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap();
    let listed = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.list",
        serde_json::json!({}),
    )
    .await
    .unwrap();
    assert_eq!(listed[0]["name"], "Tools");
    let updated = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.update",
        serde_json::json!({
            "album_id": created.id,
            "name": "Tools renamed",
            "expected_revision": created.revision
        }),
    )
    .await
    .unwrap();
    assert_eq!(updated["name"], "Tools renamed");
    let storage = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "storage.check",
        serde_json::json!({}),
    )
    .await
    .unwrap();
    assert_eq!(storage["kind"], "local");
    let rejected = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "shell.exec",
        serde_json::json!({}),
    )
    .await;
    assert!(rejected.is_err());
}

#[tokio::test]
async fn agent_turn_runs_a_local_tool_then_completes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let (local, upstream) = tokio::io::duplex(8 * 1024);
    let session_for_task = session_id.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/start"));
        assert!(!line.contains("sk-or-"));
        upstream
            .write_all(br#"{"method":"tool","params":{"name":"album.list","arguments":{}}}"#)
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
        upstream.flush().await.unwrap();
        line.clear();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/toolResult"));
        upstream
            .write_all(br#"{"method":"completed","params":{"text":"listed"}}"#)
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
        let _ = session_for_task;
    });
    let (local_read, mut local_write) = tokio::io::split(local);
    let mut local_read = tokio::io::BufReader::new(local_read);
    let text = gallery::ai::drive_turn(
        &state,
        &actor,
        &session_id,
        "turn-local",
        "list albums",
        &mut local_read,
        &mut local_write,
    )
    .await
    .unwrap();
    assert_eq!(text, "listed");
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-local'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "completed");
}

#[tokio::test]
async fn agent_turn_spawns_a_local_program() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let script = dir.path().join("upstream.py");
    std::fs::write(
        &script,
        "#!/usr/bin/env python3\nimport os, sys\nif os.environ.get('OPENROUTER_API_KEY') or os.environ.get('GALLERY_OPENROUTER_KEY'):\n    sys.exit(2)\nsys.stdin.readline()\nsys.stdout.write('{\"method\":\"tool\",\"params\":{\"name\":\"storage.check\",\"arguments\":{}}}\\n')\nsys.stdout.flush()\nsys.stdin.readline()\nsys.stdout.write('{\"method\":\"completed\",\"params\":{\"text\":\"spawned\"}}\\n')\nsys.stdout.flush()\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let text = gallery::ai::spawn_and_drive(
        &state,
        &actor,
        session["id"].as_str().unwrap(),
        "turn-spawn",
        "check storage",
        &script,
    )
    .await
    .unwrap();
    assert_eq!(text, "spawned");
}

async fn codex_process_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
}

#[tokio::test]
async fn codex_app_server_command_handshakes() {
    let _codex = codex_process_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let (program, args) = gallery::ai::codex_launch(Some(std::path::Path::new("codex")));
    assert_eq!(program, std::path::PathBuf::from("codex"));
    assert_eq!(args, vec!["app-server".to_string()]);
    let script = dir.path().join("codex");
    std::fs::write(
        &script,
        "#!/usr/bin/env python3\nimport json, sys\nassert sys.argv[1:] == ['app-server']\nhello = json.loads(sys.stdin.readline())\nassert hello['method'] == 'initialize'\nsys.stdout.write(json.dumps({'id': hello.get('id'), 'result': {'protocol': 1}}) + '\\n')\nsys.stdout.flush()\nnoted = json.loads(sys.stdin.readline())\nassert noted['method'] == 'initialized'\nstarted = json.loads(sys.stdin.readline())\nassert started['method'] == 'thread/start'\nassert 'deepseek' in started['params']['model']\nsys.stdout.write(json.dumps({'id': started.get('id'), 'result': {'thread': {'id': 'thread-1'}}}) + '\\n')\nsys.stdout.flush()\nturn = json.loads(sys.stdin.readline())\nassert turn['method'] == 'turn/start'\nassert turn['params']['threadId'] == 'thread-1'\nsys.stdout.write(json.dumps({'method': 'turn/completed', 'params': {'text': 'codex'}}) + '\\n')\nsys.stdout.flush()\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let text = gallery::ai::spawn_codex_turn(
        &state.pool,
        &actor,
        session["id"].as_str().unwrap(),
        "turn-codex",
        "hello",
        Some(&script),
    )
    .await
    .unwrap();
    assert_eq!(text, "codex");
    let thread: (Option<String>,) =
        sqlx::query_as("SELECT upstream_thread_id FROM agent_sessions WHERE id = ?")
            .bind(session["id"].as_str().unwrap())
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(thread.0.as_deref(), Some("thread-1"));
    let missing = gallery::ai::spawn_codex_turn(
        &state.pool,
        &actor,
        session["id"].as_str().unwrap(),
        "turn-missing",
        "hello",
        Some(std::path::Path::new("/nonexistent/codex")),
    )
    .await;
    assert!(missing.is_err());

    gallery::ai::reset_codex_processes().await;
    let keep = dir.path().join("codex-keep");
    std::fs::write(
        &keep,
        "#!/usr/bin/env python3\nimport json, sys\n\ndef read():\n    line = sys.stdin.readline()\n    if not line:\n        sys.exit(0)\n    return json.loads(line)\n\nhello = read()\nassert hello['method'] == 'initialize'\nsys.stdout.write(json.dumps({'id': hello.get('id'), 'result': {'protocol': 1}}) + '\\n')\nsys.stdout.flush()\nnoted = read()\nassert noted['method'] == 'initialized'\nstarted = read()\nassert started['method'] == 'thread/start'\nsys.stdout.write(json.dumps({'id': started.get('id'), 'result': {'thread': {'id': 'thread-keep'}}}) + '\\n')\nsys.stdout.flush()\nwhile True:\n    msg = read()\n    if msg.get('method') == 'turn/start':\n        sys.stdout.write(json.dumps({'method': 'turn/completed', 'params': {'text': 'kept'}}) + '\\n')\n        sys.stdout.flush()\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut kept_ids = Vec::new();
    for index in 0..3 {
        let kept = gallery::ai::create_session(
            &state.pool,
            &actor,
            true,
            gallery::ai::CreateSession { scope: None },
        )
        .await
        .unwrap();
        let id = kept["id"].as_str().unwrap().to_string();
        let text = gallery::ai::spawn_codex_turn(
            &state.pool,
            &actor,
            &id,
            &format!("turn-keep-{index}"),
            "again",
            Some(&keep),
        )
        .await
        .unwrap();
        assert_eq!(text, "kept");
        kept_ids.push(id);
    }
    let again = gallery::ai::spawn_codex_turn(
        &state.pool,
        &actor,
        &kept_ids[2],
        "turn-keep-reuse",
        "again",
        Some(std::path::Path::new("/nonexistent/codex")),
    )
    .await
    .unwrap();
    assert_eq!(again, "kept");
    assert_eq!(
        gallery::ai::codex_idle_sessions(),
        vec![kept_ids[1].clone(), kept_ids[2].clone()]
    );
    let thread: (Option<String>,) =
        sqlx::query_as("SELECT upstream_thread_id FROM agent_sessions WHERE id = ?")
            .bind(&kept_ids[0])
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(thread.0.as_deref(), Some("thread-keep"));
    gallery::ai::reset_codex_processes().await;
}

#[tokio::test]
async fn codex_cli_replies_through_openrouter_deepseek() {
    let _codex = codex_process_lock().await;
    let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".tools/codex");
    let key = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("secrets/openrouter.key");
    assert!(bin.is_file(), "codex cli should be installed at .tools/codex");
    assert!(key.is_file(), "openrouter key file");
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let home = dir.path().join("codex-home");
    std::env::set_var("GALLERY_CODEX_HOME", &home);
    std::env::set_var("GALLERY_CODEX_CWD", &work);
    std::env::set_var("GALLERY_OPENROUTER_KEY_FILE", &key);
    std::env::set_var("GALLERY_CODEX_MODEL", "deepseek/deepseek-v4.1-flash");
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().join("data");
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let text = gallery::ai::spawn_codex_turn(
        &state.pool,
        &actor,
        session["id"].as_str().unwrap(),
        "turn-live",
        "Reply with the single word pong.",
        Some(&bin),
    )
    .await
    .unwrap();
    assert!(
        text.to_ascii_lowercase().contains("pong"),
        "deepseek reply did not contain pong"
    );
}

#[tokio::test]
async fn process_exit_closes_the_bridge_and_marks_unconfirmed_input() {
    let _codex = codex_process_lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let (token,): (String,) =
        sqlx::query_as("SELECT bridge_token FROM agent_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let keep = dir.path().join("codex-bridge");
    std::fs::write(
        &keep,
        "#!/usr/bin/env python3\nimport json, sys\n\ndef read():\n    line = sys.stdin.readline()\n    if not line:\n        sys.exit(0)\n    return json.loads(line)\n\nhello = read()\nsys.stdout.write(json.dumps({'id': hello.get('id'), 'result': {'protocol': 1}}) + '\\n')\nsys.stdout.flush()\nread()\nstarted = read()\nsys.stdout.write(json.dumps({'id': started.get('id'), 'result': {'thread': {'id': 'thread-bridge'}}}) + '\\n')\nsys.stdout.flush()\nwhile True:\n    msg = read()\n    if msg.get('method') == 'turn/start':\n        sys.stdout.write(json.dumps({'method': 'turn/completed', 'params': {'text': 'held'}}) + '\\n')\n        sys.stdout.flush()\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&keep, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let text = gallery::ai::spawn_codex_turn(
        &state.pool,
        &actor,
        &session_id,
        "turn-bridge",
        "hold",
        Some(&keep),
    )
    .await
    .unwrap();
    assert_eq!(text, "held");
    let (still,): (Option<String>,) =
        sqlx::query_as("SELECT bridge_token FROM agent_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(still.as_deref(), Some(token.as_str()));
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_messages(id, session_id, seq, client_message_id, mode, delivery_state, body, created_at)
         VALUES('msg-queued', ?, 2, 'queued-bridge', 'steer_if_active', 'queued', 'later', ?),
                ('msg-kept', ?, 3, 'accepted-bridge', 'user', 'accepted', 'saved', ?)",
    )
    .bind(&session_id)
    .bind(now)
    .bind(&session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at)
         VALUES('turn-flight', ?, 1, 'running', ?)",
    )
    .bind(&session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    gallery::ai::reset_codex_processes().await;
    let (closed,): (Option<String>,) =
        sqlx::query_as("SELECT bridge_token FROM agent_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(closed.is_none());
    let queued: (String,) =
        sqlx::query_as("SELECT delivery_state FROM agent_messages WHERE id = 'msg-queued'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(queued.0, "delivery_unknown");
    let kept: (String,) =
        sqlx::query_as("SELECT delivery_state FROM agent_messages WHERE id = 'msg-kept'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(kept.0, "accepted");
    let flight: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-flight'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(flight.0, "unknown");
    let stream = tokio::net::UnixStream::connect(&state.mcp_socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    let denied = mcp_rpc(
        &mut write,
        &mut lines,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "clientInfo": {"name": "gallery-test", "version": "0"},
            "gallery": {"sessionId": session_id, "token": token}
        }),
    )
    .await;
    assert_eq!(denied["error"]["message"], "UNAUTHENTICATED");
}

#[tokio::test]
async fn silent_control_request_needs_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at) VALUES('turn-silent', ?, 1, 'running', ?)",
    )
    .bind(session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let (client, upstream) = tokio::io::duplex(1024);
    let (read_half, mut write_half) = tokio::io::split(client);
    let mut read_half = tokio::io::BufReader::new(read_half);
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        let _ = upstream.read_line(&mut line).await;
        std::future::pending::<()>().await;
    });
    gallery::ai::steer_codex_turn(&mut write_half, "thread-silent", "turn-silent", "wait")
        .await
        .unwrap();
    let err = gallery::ai::finish_interrupt(
        &state.pool,
        session_id,
        "turn-silent",
        &mut read_half,
        std::time::Duration::from_millis(60),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("NEEDS_RECONCILE"), "{err}");
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-silent'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "unknown");
    let session_state: (String,) =
        sqlx::query_as("SELECT state FROM agent_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(session_state.0, "needs_reconcile");
}

#[tokio::test]
async fn codex_steer_and_interrupt_keep_submitted_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0.clone(),
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at) VALUES('turn-stop', ?, 1, 'running', ?)",
    )
    .bind(&session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_messages(id, session_id, seq, client_message_id, mode, delivery_state, body, created_at)
         VALUES('msg-q', ?, 1, 'queued-1', 'steer_if_active', 'queued', 'later', ?)",
    )
    .bind(&session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, actor_id, available_at, created_at, updated_at)
         VALUES('job-continues', 'reconcile', 'dedupe-continues', '{}', '{}', 'queued', ?, ?, ?, ?)",
    )
    .bind(&actor.id)
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let (client, upstream) = tokio::io::duplex(8 * 1024);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/steer"));
        assert!(line.contains("expectedTurnId"));
        line.clear();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/interrupt"));
        upstream
            .write_all(br#"{"method":"turn/completed","params":{"turn":{"status":"interrupted"}}}"#)
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
        upstream.flush().await.unwrap();
    });
    let (read_half, mut write_half) = tokio::io::split(client);
    let mut read_half = tokio::io::BufReader::new(read_half);
    gallery::ai::steer_codex_turn(&mut write_half, "thread-1", "turn-stop", "focus the failing test")
        .await
        .unwrap();
    let status = gallery::ai::interrupt_codex_turn(
        &state.pool,
        &session_id,
        "turn-stop",
        "thread-1",
        &mut read_half,
        &mut write_half,
    )
    .await
    .unwrap();
    assert_eq!(status, "interrupted");
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-stop'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "interrupted");
    let message: (String,) = sqlx::query_as("SELECT delivery_state FROM agent_messages WHERE id = 'msg-q'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(message.0, "paused");
    let job = gallery::jobs::get(&state.pool, "job-continues").await.unwrap().unwrap();
    assert_eq!(job.state, "queued");
}

#[tokio::test]
async fn a_stop_recorded_in_sqlite_interrupts_the_other_process_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let (client, upstream) = tokio::io::duplex(8 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut client_read = tokio::io::BufReader::new(client_read);
        loop {
            let mut line = String::new();
            if client_read.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (seen_start, wait_start) = tokio::sync::oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/start"));
        let _ = seen_start.send(());
        line.clear();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/interrupt"));
        upstream
            .write_all(br#"{"method":"turn/completed","params":{"turn":{"status":"interrupted"}}}"#)
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
    });
    let pool = state.pool.clone();
    let drive_session = session_id.clone();
    let drive = tokio::spawn(async move {
        gallery::ai::drive_live_turn(
            &pool,
            &drive_session,
            "turn-other",
            "thread-other",
            "keep going",
            &mut lines,
            &mut { client_write },
            std::time::Duration::from_secs(2),
        )
        .await
    });
    wait_start.await.unwrap();
    sqlx::query("UPDATE agent_turns SET state = 'stop_requested' WHERE id = 'turn-other'")
        .execute(&state.pool)
        .await
        .unwrap();
    drive.await.unwrap().unwrap();
    upstream_task.await.unwrap();
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-other'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "interrupted");
}

#[tokio::test]
async fn http_stop_interrupts_the_live_codex_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0.clone(),
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, actor_id, available_at, created_at, updated_at)
         VALUES('job-live-stop', 'reconcile', 'dedupe-live-stop', '{}', '{}', 'queued', ?, ?, ?, ?)",
    )
    .bind(&actor.id)
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let (client, upstream) = tokio::io::duplex(8 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut client_read = tokio::io::BufReader::new(client_read);
        loop {
            let mut line = String::new();
            if client_read.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (seen_start, wait_start) = tokio::sync::oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/start"));
        let _ = seen_start.send(());
        line.clear();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/interrupt"));
        assert!(line.contains("thread-live"));
        upstream
            .write_all(br#"{"method":"turn/completed","params":{"turn":{"status":"interrupted"}}}"#)
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
    });
    let pool = state.pool.clone();
    let session_for_drive = session_id.clone();
    let drive = tokio::spawn(async move {
        gallery::ai::drive_live_turn(
            &pool,
            &session_for_drive,
            "turn-http",
            "thread-live",
            "describe the album",
            &mut lines,
            &mut { client_write },
            std::time::Duration::from_secs(10),
        )
        .await
    });
    wait_start.await.unwrap();
    let stopped = gallery::ai::stop_turn(&state.pool, &actor, &session_id, "turn-http")
        .await
        .unwrap();
    assert_eq!(stopped["state"], "stop_requested");
    drive.await.unwrap().unwrap();
    upstream_task.await.unwrap();
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-http'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "interrupted");
    let session_state: (String,) = sqlx::query_as("SELECT state FROM agent_sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(session_state.0, "idle");
    let job = gallery::jobs::get(&state.pool, "job-live-stop").await.unwrap().unwrap();
    assert_eq!(job.state, "queued");

    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let quiet_id = session["id"].as_str().unwrap().to_string();
    let (client, upstream) = tokio::io::duplex(8 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut client_read = tokio::io::BufReader::new(client_read);
        loop {
            let mut line = String::new();
            if client_read.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (seen_start, wait_start) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/start"));
        let _ = seen_start.send(());
        let mut line = String::new();
        let _ = upstream.read_line(&mut line).await;
        assert!(line.contains("turn/interrupt"));
        std::future::pending::<()>().await;
    });
    let pool = state.pool.clone();
    let quiet_for_drive = quiet_id.clone();
    let drive = tokio::spawn(async move {
        gallery::ai::drive_live_turn(
            &pool,
            &quiet_for_drive,
            "turn-grace",
            "thread-grace",
            "wait",
            &mut lines,
            &mut { client_write },
            std::time::Duration::from_millis(200),
        )
        .await
    });
    wait_start.await.unwrap();
    gallery::ai::stop_turn(&state.pool, &actor, &quiet_id, "turn-grace")
        .await
        .unwrap();
    let err = drive.await.unwrap().unwrap_err();
    assert!(err.to_string().contains("NEEDS_RECONCILE"));
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-grace'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "unknown");
    let session_state: (String,) = sqlx::query_as("SELECT state FROM agent_sessions WHERE id = ?")
        .bind(&quiet_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(session_state.0, "needs_reconcile");
}

#[tokio::test]
async fn agent_reply_deltas_merge_and_commit_with_the_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0.clone(),
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let (client, upstream) = tokio::io::duplex(8 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut client_read = tokio::io::BufReader::new(client_read);
        loop {
            let mut line = String::new();
            if client_read.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("turn/start"));
        for text in ["红", "桥"] {
            let line = serde_json::json!({
                "method": "item/agentMessage/delta",
                "params": {"delta": text}
            });
            upstream.write_all(&serde_json::to_vec(&line).unwrap()).await.unwrap();
            upstream.write_all(b"\n").await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        upstream
            .write_all(br#"{"method":"turn/completed","params":{"turn":{"status":"completed"}}}"#)
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
    });
    gallery::ai::drive_live_turn(
        &state.pool,
        &session_id,
        "turn-reply",
        "thread-reply",
        "name it",
        &mut lines,
        &mut { client_write },
        std::time::Duration::from_secs(10),
    )
    .await
    .unwrap();
    let body: (String,) = sqlx::query_as(
        "SELECT body FROM agent_messages WHERE session_id = ? AND client_message_id = 'assistant:turn-reply'",
    )
    .bind(&session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(body.0, "红桥");
    let payloads: Vec<(String,)> = sqlx::query_as(
        "SELECT payload_json FROM events WHERE type = 'agent.reply' AND resource_id = 'turn-reply' ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(payloads.len(), 2);
    let partial: Value = serde_json::from_str(&payloads[0].0).unwrap();
    let finished: Value = serde_json::from_str(&payloads[1].0).unwrap();
    assert_eq!(partial["partial"], true);
    assert_eq!(partial["text"], "红桥");
    assert_eq!(finished["partial"], false);
    assert_eq!(finished["text"], "红桥");
    assert_eq!(finished["state"], "completed");
    assert!(finished["message_id"].as_str().is_some());
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-reply'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "completed");
}

#[tokio::test]
async fn two_daily_reconciles_keep_one_job() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let first = gallery::storage::reconcile::ensure_daily(&pool, "backend-a")
        .await
        .unwrap()
        .unwrap();
    let second = gallery::storage::reconcile::ensure_daily(&pool, "backend-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'reconcile' AND dedupe_key = ?",
    )
    .bind(format!(
        "reconcile:backend-a:{}",
        chrono::Utc::now().format("%Y-%m-%d")
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count.0, 1);
}

#[tokio::test]
async fn daily_backup_skips_when_today_already_has_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('workers_draining', '1')
         ON CONFLICT(key) DO UPDATE SET value = '1'",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at)
         VALUES('bak-today', 'succeeded', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let id = gallery::backup::ensure_daily_backup(&state).await.unwrap();
    assert_eq!(id.as_deref(), Some("bak-today"));
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_runs")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);
    sqlx::query("DELETE FROM backup_runs").execute(&state.pool).await.unwrap();
    let started = gallery::backup::ensure_daily_backup(&state).await.unwrap().unwrap();
    let again = gallery::backup::ensure_daily_backup(&state).await.unwrap().unwrap();
    assert_eq!(started, again);
    let queued: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_runs WHERE state = 'queued'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(queued.0, 1);
    assert!(gallery::db::write_barrier(&state.pool).await.unwrap());
}

#[tokio::test]
async fn daily_reconcile_is_not_duplicated_while_maintenance_skips_it() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media"))
        .await
        .unwrap();
    let first = gallery::storage::reconcile::ensure_daily(&pool, &storage.backend_id)
        .await
        .unwrap()
        .unwrap();
    let second = gallery::storage::reconcile::ensure_daily(&pool, &storage.backend_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'reconcile' AND dedupe_key LIKE 'reconcile:%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    gallery::upgrade::enter_maintenance(&pool, "upgrade", "build-a")
        .await
        .unwrap();
    assert!(gallery::storage::reconcile::ensure_daily(&pool, &storage.backend_id)
        .await
        .unwrap()
        .is_none());
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE kind = 'reconcile'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn starved_background_job_reaches_the_front() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    sqlx::query("INSERT INTO schema_app(key, value) VALUES('queue_starve_ms', '1000')")
        .execute(&pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, priority, available_at, created_at, updated_at)
         VALUES('job-old', 'backup', 'dedupe-old', '{}', '{}', 'queued', 1, ?, ?, ?)",
    )
    .bind(now - 5_000)
    .bind(now - 5_000)
    .bind(now - 5_000)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, priority, available_at, created_at, updated_at)
         VALUES('job-new', 'generate_variant', 'dedupe-new', '{}', '{}', 'queued', 100, ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let claimed = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(claimed.0, "job-old");
    let priority: (i64,) = sqlx::query_as("SELECT priority FROM jobs WHERE id = 'job-old'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(priority.0, 100);
}

#[tokio::test]
async fn maintenance_keeps_the_write_barrier_for_a_different_image() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::storage::bootstrap_local(&pool, dir.path().join("media"))
        .await
        .unwrap();
    gallery::upgrade::enter_maintenance(&pool, "upgrade", "build-a")
        .await
        .unwrap();
    assert!(gallery::db::write_barrier(&pool).await.unwrap());
    let now = gallery::db::now_ms();
    for (id, kind) in [("job-thumb", "generate_variant"), ("job-backup", "backup")] {
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, priority, available_at, created_at, updated_at)
             VALUES(?, ?, ?, '{}', '{}', 'queued', 100, ?, ?, ?)",
        )
        .bind(id)
        .bind(kind)
        .bind(format!("dedupe-{id}"))
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    assert!(gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().is_none());
    assert!(gallery::upgrade::release_maintenance(&pool, "build-b").await.is_err());
    assert!(gallery::db::write_barrier(&pool).await.unwrap());
    assert!(gallery::upgrade::hold_foreign_image(&pool, Some("build-b")).await.unwrap());
    gallery::upgrade::release_maintenance(&pool, "build-a").await.unwrap();
    assert!(!gallery::db::write_barrier(&pool).await.unwrap());
    let claimed = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(claimed.0, "job-thumb");
}

#[tokio::test]
async fn maintenance_claim_keeps_the_current_migration_and_authorized_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media"))
        .await
        .unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE username = 'admin'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('target-keep', 'local', '{\"kind\":\"local\",\"root\":\"/tmp/gallery-keep\"}', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state, created_at, updated_at
         ) VALUES('mig-keep', ?, 'target-keep', 'forward', 'copying', 'running', ?, ?)",
    )
    .bind(&storage.backend_id)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET write_barrier = 1, current_migration_id = 'mig-keep' WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let jobs = [
        ("job-backup", "backup", "{}"),
        ("job-restore", "restore", "{}"),
        ("job-thumb", "generate_variant", "{}"),
        ("job-other", "storage_migrate_copy", "{\"migration_id\":\"mig-other\"}"),
        ("job-copy", "storage_migrate_copy", "{\"migration_id\":\"mig-keep\"}"),
        ("job-diag", "reconcile", "{}"),
        ("job-purge", "purge_media", "{}"),
    ];
    for (id, kind, scope) in jobs {
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
             VALUES(?, ?, ?, '{}', ?, 'queued', ?, ?, ?)",
        )
        .bind(id)
        .bind(kind)
        .bind(format!("dedupe-{id}"))
        .bind(scope)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    let mut claimed = Vec::new();
    for _ in 0..6 {
        match gallery::jobs::claim_next(&pool, "worker-test").await.unwrap() {
            Some(job) => claimed.push(job.0),
            None => break,
        }
    }
    claimed.sort();
    assert_eq!(claimed, vec!["job-copy".to_string(), "job-diag".to_string()]);
    let refused = gallery::migrate_storage::authorize_cleanup(&pool, &actor, "mig-keep", "backup")
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("CLEANUP_KIND"));
    let view = gallery::migrate_storage::authorize_cleanup(&pool, &actor, "mig-keep", "purge_media")
        .await
        .unwrap();
    let paused = view["writes_paused"].as_array().unwrap();
    assert!(paused.iter().all(|item| item != "purge_media" && item != "reconcile"));
    assert!(paused.iter().any(|item| item == "backup"));
    assert!(paused.iter().any(|item| item == "restore"));
    let purge = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(purge.0, "job-purge");
    assert!(gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().is_none());
    sqlx::query("UPDATE storage_route SET current_migration_id = NULL WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let backup = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(backup.0, "job-backup");
    assert!(gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().is_none());
    let waiting: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-restore'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(waiting.0, "queued");
    let other: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-other'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(other.0, "queued");
}

#[tokio::test]
async fn old_turn_receipt_does_not_idle_a_newer_turn() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, status, revision, created_at)
         VALUES('actor-1', 'ada', 'x', 'owner', 'active', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, revision, created_at, updated_at)
         VALUES('sess-1', 'actor-1', '{}', 'stopping', 2, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, revision, created_at)
         VALUES('turn-old', 'sess-1', 1, 'running', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, revision, created_at)
         VALUES('turn-new', 'sess-1', 2, 'running', 1, ?)",
    )
    .bind(now + 1)
    .execute(&pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: "actor-1".into(),
        username: "ada".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let recorded = gallery::ai::record_turn_terminal(&pool, &actor, "sess-1", "turn-old", "completed")
        .await
        .unwrap();
    assert_eq!(recorded["state"], "completed");
    let session: (String,) = sqlx::query_as("SELECT state FROM agent_sessions WHERE id = 'sess-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(session.0, "stopping");
    let newer: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-new'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(newer.0, "running");
    gallery::ai::record_turn_terminal(&pool, &actor, "sess-1", "turn-old", "failed")
        .await
        .unwrap();
    let old: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-old'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old.0, "completed");
}

#[tokio::test]
async fn session_scope_does_not_grow_after_new_albums() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media"))
        .await
        .unwrap();
    let actor: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let actor = gallery::auth::Actor {
        id: actor.0,
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let first = gallery::album::create(
        &pool,
        &actor,
        gallery::album::CreateAlbum { name: "Kept".into(), description: None },
    )
    .await
    .unwrap();
    let session = gallery::ai::create_session(
        &pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    assert_eq!(session["revision"], 1);
    assert_eq!(session["scope"]["albums"][0], first.id);
    let second = gallery::album::create(
        &pool,
        &actor,
        gallery::album::CreateAlbum { name: "Later".into(), description: None },
    )
    .await
    .unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    let state = gallery::app::AppState {
        pool: pool.clone(),
        storage,
        config: std::sync::Arc::new(cfg),
        job_hints: std::sync::Arc::new(gallery::jobs::JobHints::new()),
        inline_codec: true,
        ready: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        mcp_socket: dir.path().join("mcp.sock"),
    };
    let session_id = session["id"].as_str().unwrap();
    let visible = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.read",
        serde_json::json!({"album_id": first.id}),
    )
    .await
    .unwrap();
    assert_eq!(visible["name"], "Kept");
    let hidden = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.read",
        serde_json::json!({"album_id": second.id}),
    )
    .await;
    assert!(hidden.unwrap_err().to_string().contains("NOT_FOUND"));
    let listed = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.list",
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let ids: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert_eq!(ids, vec![first.id.as_str()]);
}

#[tokio::test]
async fn resume_does_not_run_a_committed_move_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, source) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Source"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let source_id = source["id"].as_str().unwrap().to_string();
    let (status, target) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Target"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let target_id = target["id"].as_str().unwrap().to_string();
    let image_id = upload_png(&h, &source_id, "once.png", 8).await;
    let (status, source_now) = h
        .json(Request::get(format!("/api/v1/albums/{source_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, target_now) = h
        .json(Request::get(format!("/api/v1/albums/{target_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE role = 'owner'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, revision, created_at, updated_at)
         VALUES('sess-replay', ?, '{}', 'active', 1, ?, ?)",
    )
    .bind(&owner.0)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, revision, created_at)
         VALUES('turn-replay', 'sess-replay', 1, 'running', 1, ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0.clone(),
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let args = serde_json::json!({
        "album_id": source_id,
        "target_album_id": target_id,
        "image_ids": [image_id],
        "expected_revision": source_now["revision"],
        "target_expected_revision": target_now["revision"],
    });
    gallery::ai::dispatch_tool(&state, &actor, "sess-replay", "album.move_images", args.clone())
        .await
        .unwrap();
    let placed: (String,) = sqlx::query_as(
        "SELECT album_id FROM album_images WHERE image_id = ?",
    )
    .bind(&image_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(placed.0, target_id);
    let back_source = gallery::album::get(&state.pool, Some(&actor), &target_id).await.unwrap();
    let back_target = gallery::album::get(&state.pool, Some(&actor), &source_id).await.unwrap();
    gallery::album::move_images(
        &state.pool,
        &actor,
        &target_id,
        &source_id,
        &[image_id.clone()],
        Some(back_source.revision),
        Some(back_target.revision),
        None,
    )
    .await
    .unwrap();
    gallery::ai::dispatch_tool(&state, &actor, "sess-replay", "album.move_images", args)
        .await
        .unwrap();
    let stayed: (String,) = sqlx::query_as("SELECT album_id FROM album_images WHERE image_id = ?")
        .bind(&image_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(stayed.0, source_id);
}

#[tokio::test]
async fn session_lists_committed_actions_and_running_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, status, revision, created_at)
         VALUES('actor-1', 'ada', 'x', 'owner', 'active', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, revision, created_at, updated_at)
         VALUES('sess-1', 'actor-1', '{}', 'idle', 1, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, revision, created_at)
         VALUES('turn-1', 'sess-1', 1, 'running', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_actions(id, session_id, turn_id, plan_revision, state, tool_name, result_ref, created_at)
         VALUES('act-1', 'sess-1', 'turn-1', 1, 'committed', 'album.move_images', '{\"ok\":true}', ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-1', 'generate_variant', 'dedupe-job-1', '{}', '{}', 'running', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turn_jobs(turn_id, job_id, submitted_at) VALUES('turn-1', 'job-1', ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: "actor-1".into(),
        username: "ada".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let view = gallery::ai::get_session(&pool, &actor, "sess-1").await.unwrap();
    assert_eq!(view["actions"][0]["id"], "act-1");
    assert_eq!(view["actions"][0]["state"], "committed");
    assert_eq!(view["running_jobs"][0]["id"], "job-1");
    assert_eq!(view["running_jobs"][0]["state"], "running");
    sqlx::query("UPDATE jobs SET state = 'succeeded' WHERE id = 'job-1'")
        .execute(&pool)
        .await
        .unwrap();
    let later = gallery::ai::get_session(&pool, &actor, "sess-1").await.unwrap();
    assert!(later["running_jobs"].as_array().unwrap().is_empty());
    assert_eq!(later["actions"][0]["tool_name"], "album.move_images");
}

#[tokio::test]
async fn stopped_turn_does_not_commit_the_move() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, source) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"From"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let source_id = source["id"].as_str().unwrap().to_string();
    let (status, target) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"To"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let target_id = target["id"].as_str().unwrap().to_string();
    let image_id = upload_png(&h, &source_id, "stay.png", 8).await;
    let (status, source_now) = h
        .json(Request::get(format!("/api/v1/albums/{source_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, target_now) = h
        .json(Request::get(format!("/api/v1/albums/{target_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE role = 'owner'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, revision, created_at, updated_at)
         VALUES('sess-stop', ?, '{}', 'idle', 1, ?, ?)",
    )
    .bind(&owner.0)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, revision, created_at)
         VALUES('turn-stop', 'sess-stop', 1, 'stop_requested', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let moved = gallery::album::move_images(
        &pool,
        &actor,
        &source_id,
        &target_id,
        &[image_id.clone()],
        source_now["revision"].as_i64(),
        target_now["revision"].as_i64(),
        Some("sess-stop"),
    )
    .await;
    assert!(moved.unwrap_err().to_string().contains("TURN_STOPPED"));
    let renamed = gallery::album::patch(
        &pool,
        &actor,
        &source_id,
        gallery::album::PatchAlbum {
            name: Some("Renamed".into()),
            description: None,
            cover_image_id: None,
            expected_revision: source_now["revision"].as_i64(),
        },
        Some("sess-stop"),
    )
    .await;
    assert!(renamed.unwrap_err().to_string().contains("TURN_STOPPED"));
    let (status, still) = h
        .json(Request::get(format!("/api/v1/albums/{source_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(still["name"], "From");
    let (status, images) = h
        .json(
            Request::get(format!("/api/v1/albums/{source_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(images["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == image_id));
}

#[tokio::test]
async fn activate_rejects_a_candidate_with_broken_foreign_keys() {
    use sqlx::Connection;
    use std::str::FromStr;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.sqlite");
    let pool = gallery::db::connect(&source).await.unwrap();
    let (enabled,): (i64,) = sqlx::query_as("PRAGMA foreign_keys").fetch_one(&pool).await.unwrap();
    assert_eq!(enabled, 1);
    let prepared = gallery::upgrade::prepare(dir.path(), &source).await.unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(&format!(
        "sqlite://{}",
        prepared.candidate_db.display()
    ))
    .unwrap()
    .foreign_keys(false);
    let mut conn = sqlx::sqlite::SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query(
        "INSERT INTO album_images(album_id, image_id, sort_key, added_at)
         VALUES('missing-album', 'missing-image', 1, 1)",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    drop(conn);
    let refused = gallery::upgrade::activate(dir.path()).await.unwrap_err();
    assert!(refused.to_string().contains("foreign_key_check"));
    assert_eq!(
        gallery::upgrade::load(dir.path()).unwrap().active,
        gallery::upgrade::ActiveDb::Previous
    );
    let (still_on,): (i64,) = sqlx::query_as("PRAGMA foreign_keys").fetch_one(&pool).await.unwrap();
    assert_eq!(still_on, 1);
}

#[tokio::test]
async fn upgrade_plan_rejects_a_changed_migration_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("gallery.sqlite");
    let pool = gallery::db::connect(&db).await.unwrap();
    sqlx::query("INSERT INTO schema_app(key, value) VALUES('plan_sentinel', '1')")
        .execute(&pool)
        .await
        .unwrap();
    let report = gallery::upgrade::plan(&db).await.unwrap();
    assert!(report.applied.contains(&1));
    assert!(report.applied.contains(&9));
    assert!(report.pending.is_empty());
    assert!(!report.sqlite_version.is_empty());
    sqlx::query("UPDATE _sqlx_migrations SET checksum = zeroblob(32) WHERE version = 1")
        .execute(&pool)
        .await
        .unwrap();
    let refused = gallery::upgrade::plan(&db).await.unwrap_err();
    assert!(refused.to_string().contains("checksum"));
    let sentinel: (String,) = sqlx::query_as("SELECT value FROM schema_app WHERE key = 'plan_sentinel'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sentinel.0, "1");
}

#[tokio::test]
async fn upgrade_failure_keeps_previous_generation() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    gallery::app::bootstrap(cfg.clone(), true).await.unwrap();
    let prepared = gallery::upgrade::prepare(&cfg.data_dir, &cfg.db_path())
        .await
        .unwrap();
    assert_eq!(prepared.active, gallery::upgrade::ActiveDb::Previous);
    let failed = gallery::upgrade::apply(&cfg.data_dir, "bad", "NOT VALID SQL").await;
    assert!(failed.is_err());
    let after_fail = gallery::upgrade::load(&cfg.data_dir).unwrap();
    assert_eq!(after_fail.active, gallery::upgrade::ActiveDb::Previous);
    assert_eq!(after_fail.status, "apply_failed");
    gallery::upgrade::apply(
        &cfg.data_dir,
        "probe",
        "CREATE TABLE upgrade_probe(id INTEGER PRIMARY KEY)",
    )
    .await
    .unwrap();
    let activated = gallery::upgrade::activate(&cfg.data_dir).await.unwrap();
    assert_eq!(activated.active, gallery::upgrade::ActiveDb::Candidate);
    let rolled = gallery::upgrade::rollback(&cfg.data_dir).unwrap();
    assert_eq!(rolled.active, gallery::upgrade::ActiveDb::Previous);
    assert_eq!(rolled.status, "rolled_back");
    gallery::upgrade::activate(&cfg.data_dir).await.unwrap();
    gallery::upgrade::open_writes(&cfg.data_dir).unwrap();
    assert!(gallery::upgrade::rollback(&cfg.data_dir).is_err());
}

#[tokio::test]
async fn mcp_socket_lists_album_and_blocks_writes_after_stop() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let created = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum {
            name: "Bridge".into(),
            description: None,
        },
    )
    .await
    .unwrap();
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let (token,): (String,) =
        sqlx::query_as("SELECT bridge_token FROM agent_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    let stream = tokio::net::UnixStream::connect(&state.mcp_socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    let hello = mcp_rpc(
        &mut write,
        &mut lines,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "clientInfo": {"name": "gallery-test", "version": "0"},
            "gallery": {"sessionId": session_id, "token": token}
        }),
    )
    .await;
    assert_eq!(hello["result"]["serverInfo"]["name"], "gallery");
    assert!(!hello.to_string().contains("sk-or-"), "bridge response leaked a credential");
    let tools = mcp_rpc(&mut write, &mut lines, 2, "tools/list", serde_json::json!({})).await;
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"album.list"));
    assert!(!names.contains(&"shell.exec"));
    let listed = mcp_rpc(
        &mut write,
        &mut lines,
        3,
        "tools/call",
        serde_json::json!({"name": "album.list", "arguments": {}}),
    )
    .await;
    let albums: serde_json::Value =
        serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(albums[0]["name"], "Bridge");
    assert_eq!(listed["result"]["isError"], false);
    let updated = mcp_rpc(
        &mut write,
        &mut lines,
        4,
        "tools/call",
        serde_json::json!({
            "name": "album.update",
            "arguments": {
                "album_id": created.id,
                "name": "Bridge renamed",
                "expected_revision": created.revision
            }
        }),
    )
    .await;
    assert_eq!(updated["result"]["isError"], false);
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at) VALUES('turn-mcp', ?, 1, 'running', ?)",
    )
    .bind(&session_id)
    .bind(gallery::db::now_ms())
    .execute(&state.pool)
    .await
    .unwrap();
    gallery::ai::stop_turn(&state.pool, &actor, &session_id, "turn-mcp")
        .await
        .unwrap();
    let blocked = mcp_rpc(
        &mut write,
        &mut lines,
        5,
        "tools/call",
        serde_json::json!({
            "name": "album.update",
            "arguments": {"album_id": created.id, "name": "nope", "expected_revision": created.revision + 1}
        }),
    )
    .await;
    assert_eq!(blocked["result"]["isError"], true);
    assert_eq!(blocked["result"]["content"][0]["text"], "EPOCH");
    let still = mcp_rpc(
        &mut write,
        &mut lines,
        6,
        "tools/call",
        serde_json::json!({"name": "album.list", "arguments": {}}),
    )
    .await;
    assert_eq!(still["result"]["isError"], false);
    sqlx::query("UPDATE agent_turns SET state = 'completed' WHERE id = 'turn-mcp'")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at) VALUES('turn-next', ?, 1, 'running', ?)",
    )
    .bind(&session_id)
    .bind(gallery::db::now_ms())
    .execute(&state.pool)
    .await
    .unwrap();
    let stale = mcp_rpc(
        &mut write,
        &mut lines,
        8,
        "tools/call",
        serde_json::json!({
            "name": "album.update",
            "arguments": {"album_id": created.id, "name": "still old", "expected_revision": created.revision + 1}
        }),
    )
    .await;
    assert_eq!(stale["result"]["content"][0]["text"], "EPOCH");
    let (epoch,): (i64,) = sqlx::query_as("SELECT run_epoch FROM agent_sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(epoch, 2);
    let fresh = tokio::net::UnixStream::connect(&state.mcp_socket).await.unwrap();
    let (fresh_read, mut fresh_write) = fresh.into_split();
    let mut fresh_lines = tokio::io::BufReader::new(fresh_read).lines();
    let welcomed = mcp_rpc(
        &mut fresh_write,
        &mut fresh_lines,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "clientInfo": {"name": "gallery-test", "version": "0"},
            "gallery": {"sessionId": session_id, "token": token, "epoch": 2}
        }),
    )
    .await;
    assert_eq!(welcomed["result"]["serverInfo"]["name"], "gallery");
    let renamed = mcp_rpc(
        &mut fresh_write,
        &mut fresh_lines,
        2,
        "tools/call",
        serde_json::json!({
            "name": "album.update",
            "arguments": {"album_id": created.id, "name": "current epoch", "expected_revision": created.revision + 1}
        }),
    )
    .await;
    assert_eq!(renamed["result"]["isError"], false);
    sqlx::query("UPDATE actors SET must_change_password = 1 WHERE role = 'owner'")
        .execute(&state.pool)
        .await
        .unwrap();
    let locked = mcp_rpc(
        &mut write,
        &mut lines,
        7,
        "tools/call",
        serde_json::json!({"name": "album.list", "arguments": {}}),
    )
    .await;
    assert_eq!(locked["result"]["content"][0]["text"], "MUST_CHANGE_PASSWORD");
    let bad = tokio::net::UnixStream::connect(&state.mcp_socket).await.unwrap();
    let (bad_read, mut bad_write) = bad.into_split();
    let mut bad_lines = tokio::io::BufReader::new(bad_read).lines();
    let rejected = mcp_rpc(
        &mut bad_write,
        &mut bad_lines,
        1,
        "initialize",
        serde_json::json!({"gallery": {"sessionId": session_id, "token": "wrong-token"}}),
    )
    .await;
    assert_eq!(rejected["error"]["message"], "UNAUTHENTICATED");
}

async fn mcp_rpc(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    id: i64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    use tokio::io::AsyncWriteExt;
    let request = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    write
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    write.flush().await.unwrap();
    let line = lines.next_line().await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn bulk_move_needs_approval_and_paused_messages_can_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let source = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum { name: "Source".into(), description: None },
    )
    .await
    .unwrap();
    let target = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum { name: "Target".into(), description: None },
    )
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    let mut ids = Vec::new();
    for i in 0..12 {
        let image_id = format!("img-{i}");
        let sha = format!("{i:064x}");
        sqlx::query(
            "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at) VALUES(?, ?, 4, 'image/png', 'verified', 'original', ?)",
        )
        .bind(format!("blob-{i}"))
        .bind(&sha)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
             VALUES(?, ?, ?, ?, 8, 8, 'image/png', ?, ?)",
        )
        .bind(&image_id)
        .bind(&sha)
        .bind(format!("blob-{i}"))
        .bind(format!("{image_id}.png"))
        .bind(i + 1)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
        ids.push(image_id);
    }
    gallery::album::add_images(&state.pool, &actor, &source.id, &ids, Some(source.revision))
        .await
        .unwrap();
    let source = gallery::album::get(&state.pool, Some(&actor), &source.id).await.unwrap();
    let target = gallery::album::get(&state.pool, Some(&actor), &target.id).await.unwrap();
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap();
    let one = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.move_images",
        serde_json::json!({
            "album_id": source.id,
            "target_album_id": target.id,
            "image_ids": [ids[0]],
            "expected_revision": source.revision,
            "target_expected_revision": target.revision
        }),
    )
    .await
    .unwrap();
    assert!(one.get("action_id").is_none());
    let source = gallery::album::get(&state.pool, Some(&actor), &source.id).await.unwrap();
    let target = gallery::album::get(&state.pool, Some(&actor), &target.id).await.unwrap();
    let proposed = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.move_images",
        serde_json::json!({
            "album_id": source.id,
            "target_album_id": target.id,
            "image_ids": &ids[1..],
            "expected_revision": source.revision,
            "target_expected_revision": target.revision
        }),
    )
    .await
    .unwrap();
    assert_eq!(proposed["state"], "proposed");
    assert_eq!(proposed["summary"]["image_count"], 11);
    let action_id = proposed["action_id"].as_str().unwrap();
    let on_target: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM album_images WHERE album_id = ?")
        .bind(&target.id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(on_target.0, 1);
    sqlx::query("UPDATE agent_actions SET expires_at = 1 WHERE id = ?")
        .bind(action_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let expired = gallery::ai::approve_action(&state.pool, &actor, session_id, action_id, 1).await;
    assert!(expired.unwrap_err().to_string().contains("ACTION_EXPIRED"));
    sqlx::query("UPDATE agent_actions SET expires_at = ? WHERE id = ?")
        .bind(gallery::db::now_ms() + 60_000)
        .bind(action_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let approved = gallery::ai::approve_action(&state.pool, &actor, session_id, action_id, 1)
        .await
        .unwrap();
    assert_eq!(approved["state"], "committed");
    let again = gallery::ai::approve_action(&state.pool, &actor, session_id, action_id, 1)
        .await
        .unwrap();
    assert_eq!(again["state"], "committed");
    let on_target: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM album_images WHERE album_id = ?")
        .bind(&target.id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(on_target.0, 12);
    let source = gallery::album::get(&state.pool, Some(&actor), &source.id).await.unwrap();
    let target = gallery::album::get(&state.pool, Some(&actor), &target.id).await.unwrap();
    let back = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "album.move_images",
        serde_json::json!({
            "album_id": target.id,
            "target_album_id": source.id,
            "image_ids": ids,
            "expected_revision": target.revision,
            "target_expected_revision": source.revision
        }),
    )
    .await
    .unwrap();
    let rejected = gallery::ai::reject_action(
        &state.pool,
        &actor,
        session_id,
        back["action_id"].as_str().unwrap(),
        1,
    )
    .await
    .unwrap();
    assert_eq!(rejected["state"], "rejected");
    let still: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM album_images WHERE album_id = ?")
        .bind(&target.id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(still.0, 12);
    gallery::ai::post_message(
        &state.pool,
        &actor,
        session_id,
        gallery::ai::IncomingMessage {
            client_message_id: "msg-1".into(),
            text: "hold this".into(),
            mode: Some("next_turn".into()),
            expected_turn_id: None,
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at) VALUES('turn-live', ?, 1, 'running', ?)",
    )
    .bind(session_id)
    .bind(gallery::db::now_ms())
    .execute(&state.pool)
    .await
    .unwrap();
    let busy = gallery::ai::archive_session(&state.pool, &actor, session_id, None).await;
    assert!(busy.unwrap_err().to_string().contains("AGENT_BUSY"));
    let stopped = gallery::ai::stop_turn(&state.pool, &actor, session_id, "turn-live")
        .await
        .unwrap();
    assert_eq!(stopped["state"], "stop_requested");
    let still_busy = gallery::ai::archive_session(&state.pool, &actor, session_id, None).await;
    assert!(still_busy.unwrap_err().to_string().contains("AGENT_BUSY"));
    gallery::ai::record_turn_terminal(&state.pool, &actor, session_id, "turn-live", "interrupted")
        .await
        .unwrap();
    let resumed = gallery::ai::resume_pending(&state.pool, &actor, session_id).await.unwrap();
    assert_eq!(resumed["resumed"], 1);
    let queued: (String,) = sqlx::query_as(
        "SELECT delivery_state FROM agent_messages WHERE session_id = ? AND client_message_id = 'msg-1'",
    )
    .bind(session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(queued.0, "queued");
    let archived = gallery::ai::archive_session(&state.pool, &actor, session_id, None)
        .await
        .unwrap();
    assert_eq!(archived["state"], "archived");
    let closed = gallery::ai::post_message(
        &state.pool,
        &actor,
        session_id,
        gallery::ai::IncomingMessage {
            client_message_id: "msg-2".into(),
            text: "too late".into(),
            mode: None,
            expected_turn_id: None,
        },
    )
    .await;
    assert!(closed.unwrap_err().to_string().contains("SESSION_ARCHIVED"));
}

#[tokio::test]
async fn must_change_password_blocks_gallery_until_password_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Harness::new(dir.path().to_path_buf()).await;
    owner.login().await;
    let (status, _) = owner
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"ada","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    owner.cookie.clear();
    owner.csrf.clear();
    let (status, headers, body) = owner
        .call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"ada","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    owner.cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("gallery_session=")
        .unwrap()
        .to_string();
    owner.csrf = serde_json::from_slice::<Value>(&body).unwrap()["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, session) = owner
        .json(Request::get("/api/v1/session").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["actor"]["must_change_password"], true);
    let (status, blocked) = owner
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(blocked["code"], "MUST_CHANGE_PASSWORD");
    let (status, _, _) = owner
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, session) = owner
        .json(Request::get("/api/v1/session").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(session["actor"].is_null());
    let (status, _) = owner
        .json(Request::get("/api/v1/admin/users").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    owner.cookie.clear();
    owner.csrf.clear();
    let (status, headers, body) = owner
        .call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"ada","password":"collab-pass-13"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    owner.cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("gallery_session=")
        .unwrap()
        .to_string();
    owner.csrf = serde_json::from_slice::<Value>(&body).unwrap()["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = owner
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn event_cursor_gap_resyncs_and_hides_other_actors() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let owner = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let first = gallery::events::emit(
        &state.pool,
        "album.created",
        "album",
        Some("album-1"),
        Some("album-1"),
        Some(1),
        Some(&owner.id),
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let second = gallery::events::emit(
        &state.pool,
        "album.created",
        "album",
        Some("album-2"),
        Some("album-2"),
        Some(1),
        Some(&owner.id),
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let app = gallery::scheduler::router(state.clone());
    let (token, _) = gallery::auth::issue_session(&state.pool, &owner, 60_000)
        .await
        .unwrap();
    let replay = sse_body(
        &app,
        &token,
        Request::get("/api/v1/events?after=0").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(replay.contains("album.created"));
    assert!(!replay.contains("event: resync"));
    gallery::events::prune_through(&state.pool, first).await.unwrap();
    let resync = sse_body(
        &app,
        &token,
        Request::get(format!("/api/v1/events?after={first}"))
            .header("last-event-id", "0")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(resync.contains("event: resync"));
    assert!(!resync.contains("album-1"));
    let continued = sse_body(
        &app,
        &token,
        Request::get("/api/v1/events?after=0")
            .header("last-event-id", first.to_string())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(continued.contains("album-2"));
    assert!(!continued.contains("event: resync"));
    assert!(second > first);
    let quiet = sse_body(
        &app,
        &token,
        Request::get("/api/v1/events").body(Body::empty()).unwrap(),
    )
    .await;
    assert!(!quiet.contains("album.created"));
    let now = gallery::db::now_ms();
    let collab_id = "actor-collab";
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, status, must_change_password, revision, created_at)
         VALUES(?, 'bea', 'x', 'collaborator', 'active', 0, 1, ?)",
    )
    .bind(collab_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let collab = gallery::auth::Actor {
        id: collab_id.into(),
        username: "bea".into(),
        role: "collaborator".into(),
        must_change_password: false,
        revision: 1,
    };
    let (collab_token, _) = gallery::auth::issue_session(&state.pool, &collab, 60_000)
        .await
        .unwrap();
    let hidden = sse_body(
        &app,
        &collab_token,
        Request::get(format!("/api/v1/events?after={first}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(!hidden.contains("album-2"));
    let app_live = app.clone();
    let token_live = token.clone();
    let live = tokio::spawn(async move {
        sse_read(
            &app_live,
            &token_live,
            Request::get(format!("/api/v1/events?after={second}"))
                .body(Body::empty())
                .unwrap(),
            std::time::Duration::from_secs(3),
            Some("album.updated"),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    gallery::events::emit(
        &state.pool,
        "album.updated",
        "album",
        Some("album-2"),
        Some("album-2"),
        Some(2),
        Some(&owner.id),
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let live = live.await.unwrap();
    assert!(live.contains("album.updated"));
    assert!(!live.contains("event: resync"));
}

async fn sse_body(app: &axum::Router, token: &str, req: Request<Body>) -> String {
    sse_read(app, token, req, std::time::Duration::from_millis(250), None).await
}

async fn sse_read(
    app: &axum::Router,
    token: &str,
    mut req: Request<Body>,
    total: std::time::Duration,
    wait_for: Option<&str>,
) -> String {
    req.headers_mut().insert(
        header::COOKIE,
        format!("gallery_session={token}").parse().unwrap(),
    );
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut body = res.into_body();
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + total;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let slice = std::time::Duration::from_millis(200).min(deadline.saturating_duration_since(now));
        match tokio::time::timeout(slice, http_body_util::BodyExt::frame(&mut body)).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    buf.extend_from_slice(&data);
                }
                if wait_for.is_some_and(|needle| String::from_utf8_lossy(&buf).contains(needle)) {
                    break;
                }
            }
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => {
                if wait_for.is_none() {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn agent_scan_and_archive_keep_jobs_after_stop() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    cfg.ai_enabled = true;
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let album = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum {
            name: "Packed".into(),
            description: None,
        },
    )
    .await
    .unwrap();
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at) VALUES('turn-long', ?, 1, 'running', ?)",
    )
    .bind(session_id)
    .bind(gallery::db::now_ms())
    .execute(&state.pool)
    .await
    .unwrap();
    let scan = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "scans.request",
        serde_json::json!({"scope": {}}),
    )
    .await
    .unwrap();
    assert!(scan["scan_id"].as_str().unwrap().len() > 8);
    assert!(scan["job_ids"].as_array().unwrap().is_empty());
    let archive = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "archive.request",
        serde_json::json!({"album_id": album.id}),
    )
    .await
    .unwrap();
    let job_id = archive["job_id"].as_str().unwrap().to_string();
    let linked: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_turn_jobs WHERE turn_id = 'turn-long' AND job_id = ?",
    )
    .bind(&job_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(linked.0, 1);
    gallery::ai::stop_turn(&state.pool, &actor, session_id, "turn-long")
        .await
        .unwrap();
    let job = gallery::jobs::get(&state.pool, &job_id).await.unwrap().unwrap();
    assert_ne!(job.state, "canceled");
    let blocked = gallery::ai::dispatch_tool(
        &state,
        &actor,
        session_id,
        "scans.request",
        serde_json::json!({"scope": {}}),
    )
    .await;
    assert!(blocked.unwrap_err().to_string().contains("TURN_STOPPED"));
}

#[tokio::test]
async fn codex_resume_reconciles_turns_and_keeps_unknown_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0.clone(),
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let missing = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let missing_id = missing["id"].as_str().unwrap();
    let (client, _upstream) = tokio::io::duplex(1024);
    let (read, mut write) = tokio::io::split(client);
    let mut read = tokio::io::BufReader::new(read);
    let denied = gallery::ai::resume_codex_thread(
        &state.pool,
        &actor,
        missing_id,
        &mut read,
        &mut write,
    )
    .await;
    assert!(denied.unwrap_err().to_string().contains("THREAD_MISSING"));
    let session = gallery::ai::create_session(
        &state.pool,
        &actor,
        true,
        gallery::ai::CreateSession { scope: None },
    )
    .await
    .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE agent_sessions SET upstream_thread_id = 'thread-9' WHERE id = ?")
        .bind(&session_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, upstream_turn_id, generation, state, created_at)
         VALUES('turn-seen', ?, 'turn-up', 1, 'running', ?),
                ('turn-lost', ?, NULL, 1, 'running', ?)",
    )
    .bind(&session_id)
    .bind(now)
    .bind(&session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_messages(id, session_id, seq, client_message_id, mode, delivery_state, body, created_at)
         VALUES('msg-unknown', ?, 1, 'client-unknown', 'steer_if_active', 'delivery_unknown', 'maybe sent', ?)",
    )
    .bind(&session_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, actor_id, available_at, created_at, updated_at)
         VALUES('job-resume', 'reconcile', 'dedupe-resume', '{}', '{}', 'queued', ?, ?, ?, ?)",
    )
    .bind(&actor.id)
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    let (client, upstream) = tokio::io::duplex(8 * 1024);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut upstream = tokio::io::BufReader::new(upstream);
        let mut line = String::new();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("thread/resume"));
        assert!(line.contains("thread-9"));
        upstream
            .write_all(br#"{"jsonrpc":"2.0","id":33,"result":{"thread":{"id":"thread-9"}}}"#)
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
        upstream.flush().await.unwrap();
        line.clear();
        upstream.read_line(&mut line).await.unwrap();
        assert!(line.contains("thread/turns/list"));
        upstream
            .write_all(
                br#"{"jsonrpc":"2.0","id":34,"result":{"turns":[{"id":"turn-up","status":"completed"}]}}"#,
            )
            .await
            .unwrap();
        upstream.write_all(b"\n").await.unwrap();
        upstream.flush().await.unwrap();
    });
    let (read, mut write) = tokio::io::split(client);
    let mut read = tokio::io::BufReader::new(read);
    let resumed = gallery::ai::resume_codex_thread(
        &state.pool,
        &actor,
        &session_id,
        &mut read,
        &mut write,
    )
    .await
    .unwrap();
    assert_eq!(resumed["thread_id"], "thread-9");
    assert_eq!(resumed["delivery_unknown"], 1);
    assert_eq!(resumed["unknown_turns"][0], "turn-lost");
    let seen: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-seen'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(seen.0, "completed");
    let lost: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-lost'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(lost.0, "unknown");
    let session_state: (String,) =
        sqlx::query_as("SELECT state FROM agent_sessions WHERE id = ?")
            .bind(&session_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(session_state.0, "needs_reconcile");
    gallery::ai::resume_pending(&state.pool, &actor, &session_id)
        .await
        .unwrap();
    let delivery: (String,) = sqlx::query_as(
        "SELECT delivery_state FROM agent_messages WHERE id = 'msg-unknown'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(delivery.0, "delivery_unknown");
    let job = gallery::jobs::get(&state.pool, "job-resume").await.unwrap().unwrap();
    assert_eq!(job.state, "queued");
}

#[tokio::test]
async fn patch_and_backup_replay_the_original_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, created) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Original"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = created["id"].as_str().unwrap();
    let revision = created["revision"].as_i64().unwrap();
    let patch = format!(r#"{{"name":"Renamed","expected_revision":{revision}}}"#);
    let first = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "rename-1")
                .body(Body::from(patch.clone()))
                .unwrap(),
        )
        .await;
    let second = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "rename-1")
                .body(Body::from(patch))
                .unwrap(),
        )
        .await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(first.1["revision"], second.1["revision"]);
    let (status, conflict) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "rename-1")
                .body(Body::from(format!(
                    r#"{{"name":"Different","expected_revision":{revision}}}"#
                )))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "IDEMPOTENCY_CONFLICT");
    let (status, live) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(live["name"], "Renamed");
    assert_eq!(live["revision"], first.1["revision"]);
    let (status, backup) = h
        .json(
            Request::post("/api/v1/admin/backups")
                .header("idempotency-key", "backup-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (again, replay) = h
        .json(
            Request::post("/api/v1/admin/backups")
                .header("idempotency-key", "backup-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(again, StatusCode::ACCEPTED);
    assert_eq!(backup["id"], replay["id"]);
}

#[tokio::test]
async fn album_revision_accepts_if_match_and_rejects_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, _, body) = h
        .call(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Tagged"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&body).unwrap();
    let album_id = created["id"].as_str().unwrap();
    let (status, headers, _) = h
        .call(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    assert_eq!(etag, format!("\"{}\"", created["revision"].as_i64().unwrap()));
    let (status, headers, body) = h
        .call(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag)
                .body(Body::from(r#"{"name":"From tag"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let patched: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(patched["name"], "From tag");
    let next = headers.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let (status, _, body) = h
        .call(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, format!("\"{}\"", created["revision"].as_i64().unwrap()))
                .body(Body::from(r#"{"name":"Stale"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["code"], "REVISION_CONFLICT");
    let (status, _, body) = h
        .call(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Missing"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["code"], "REVISION_REQUIRED");
    let (status, _, body) = h
        .call(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, &next)
                .body(Body::from(r#"{"name":"Split","expected_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["code"], "REVISION_MISMATCH");
    let revision = next.trim_matches('"');
    let (status, _, body) = h
        .call(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, &next)
                .body(Body::from(format!(r#"{{"name":"Both","expected_revision":{revision}}}"#)))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["name"], "Both");
}

#[tokio::test]
async fn caption_jobs_wait_until_tomorrow_when_the_daily_budget_is_spent() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    cfg.ai_enabled = true;
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::db::kv_set(&mut tx, "ai_daily_token_budget", "10")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO events(type, scope_kind, payload_json, occurred_at)
         VALUES('caption.model_written', 'image', '{\"prompt_tokens\":8,\"completion_tokens\":8}', ?)",
    )
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-budget', 'caption_image', 'dedupe-budget', '{}', '{}', 'queued', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .unwrap();
    state.wake_jobs();
    let mut parked = None;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let row = sqlx::query_as::<_, (String, i64, Option<String>)>(
            "SELECT state, available_at, failure_code FROM jobs WHERE id = 'job-budget'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if row.0 == "retry_wait" && row.2.as_deref() == Some("AI_BUDGET") {
            parked = Some(row);
            break;
        }
    }
    let (state_name, available_at, _) = parked.expect("caption job should wait for the next budget day");
    assert_eq!(state_name, "retry_wait");
    assert!(available_at > now + 60 * 60 * 1000);
}

#[tokio::test]
async fn owner_library_serves_unassigned_originals_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Harness::new(dir.path().to_path_buf()).await;
    owner.login().await;
    let (status, album) = owner
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Loose"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(16, 16, false);
    let (status, batch) = owner
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = owner
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap();
    let (status, current) = owner
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = owner
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/items/remove"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "expected_revision": current["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = owner
        .call(
            Request::get(format!("/api/v1/library/images/{image_id}/original-view"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, png);
    let (status, headers, downloaded) = owner
        .call(
            Request::get(format!("/api/v1/library/images/{image_id}/original-download"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, png);
    assert!(headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("a.png"));
    owner.cookie.clear();
    let (status, _, _) = owner
        .call(
            Request::get(format!("/api/v1/library/images/{image_id}/original-view"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    owner.login().await;
    let (status, _) = owner
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    owner.cookie.clear();
    owner.csrf.clear();
    let (status, headers, body) = owner
        .call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    owner.cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("gallery_session=")
        .unwrap()
        .to_string();
    owner.csrf = serde_json::from_slice::<Value>(&body).unwrap()["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _, _) = owner
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    owner.cookie.clear();
    owner.csrf.clear();
    let (status, headers, _body) = owner
        .call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-13"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    owner.cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("gallery_session=")
        .unwrap()
        .to_string();
    let (status, _, _) = owner
        .call(
            Request::get(format!("/api/v1/library/images/{image_id}/original-view"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    owner.login().await;
    let (status, current) = owner
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = owner
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/items/add"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "expected_revision": current["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = owner
        .call(
            Request::get(format!("/api/v1/library/images/{image_id}/original-view"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn adopt_session(h: &mut Harness, headers: &header::HeaderMap, body: &[u8]) {
    h.cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("gallery_session=")
        .unwrap()
        .to_string();
    h.csrf = serde_json::from_slice::<Value>(body).unwrap()["csrf_token"]
        .as_str()
        .unwrap()
        .to_string();
}

async fn login_as(h: &mut Harness, username: &str, password: &str) {
    h.cookie.clear();
    h.csrf.clear();
    let (status, headers, body) = h
        .call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"username":"{username}","password":"{password}"}}"#
                )))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    adopt_session(h, &headers, &body);
}

#[tokio::test]
async fn collaborator_without_grant_cannot_see_private_album_or_original() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"HiddenPond"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let png = png_bytes(12, 12, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"secret.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();
    let (status, created_user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let collab_id = created_user["id"].as_str().unwrap().to_string();
    login_as(&mut h, "bea", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "bea", "collab-pass-13").await;
    for path in [
        format!("/api/v1/albums/{album_id}"),
        format!("/api/v1/albums/{album_id}/images"),
        format!("/api/v1/albums/{album_id}/images/{image_id}/original-view"),
        format!("/api/v1/albums/{album_id}/images/{image_id}/original-download"),
        "/api/v1/library/images".into(),
        "/api/v1/trash".into(),
    ] {
        let (status, _, _) = h
            .call(Request::get(&path).body(Body::empty()).unwrap())
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, found) = h
        .json(
            Request::get("/api/v1/search?q=HiddenPond")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(found["items"]["albums"].as_array().unwrap().is_empty());
    assert!(found["items"]["images"].as_array().unwrap().is_empty());
    h.login().await;
    let (status, current) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, granted) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": current["revision"],
                        "grants": [{
                            "actor_id": collab_id,
                            "browse": true,
                            "edit": false,
                            "upload": false,
                            "view_original": false,
                            "download": false
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let revision_after_grant = granted["revision"].as_i64().unwrap();
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, visible) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(visible["capabilities"]["browse"], true);
    assert_eq!(visible["capabilities"]["view_original"], false);
    assert_eq!(visible["capabilities"]["download"], false);
    let (status, images) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(images["items"][0]["id"], image_id);
    for path in [
        format!("/api/v1/albums/{album_id}/images/{image_id}/original-view"),
        format!("/api/v1/albums/{album_id}/images/{image_id}/original-download"),
    ] {
        let (status, _, _) = h
            .call(Request::get(&path).body(Body::empty()).unwrap())
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    h.login().await;
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": revision_after_grant,
                        "grants": [{
                            "actor_id": collab_id,
                            "browse": false
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, _, _) = h
        .call(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repeat_upload_returns_the_same_image_and_past_range_is_416() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Once"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let put = |bytes: Vec<u8>| {
        Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
            .header(header::CONTENT_TYPE, "image/png")
            .body(Body::from(bytes))
            .unwrap()
    };
    let (status, first) = h.json(put(png.clone())).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, album_after) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, rejected) = h.json(put(png_bytes(9, 9, false))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rejected["code"], "ITEM_SHA");
    let (status, second) = h.json(put(png.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["image_id"], second["image_id"]);
    assert_eq!(first["job_ids"], second["job_ids"]);
    assert!(!first["job_ids"].as_array().unwrap().is_empty());
    let (status, album_again) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(album_after["revision"], album_again["revision"]);
    let image_id = first["image_id"].as_str().unwrap();
    let (status, _, _) = h
        .call(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/original-view"
            ))
            .header(header::RANGE, "bytes=1000000-1000001")
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn unsafe_original_view_uses_the_derivative() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Unsafe"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "mark.png", 400).await;
    wait_thumb(&h, album_id).await;
    let (status, safe) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(safe["can_inline_original"], true);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query("UPDATE images SET original_mime = 'image/svg+xml' WHERE id = ?")
        .bind(&image_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, detail) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["can_inline_original"], false);
    let view = format!("/api/v1/albums/{album_id}/images/{image_id}/original-view");
    let (status, headers, shown) = h
        .call(Request::get(&view).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/png");
    let download = format!("/api/v1/albums/{album_id}/images/{image_id}/original-download");
    let (status, headers, raw) = h
        .call(Request::get(&download).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.starts_with("attachment;"));
    assert_ne!(shown, raw);
}

#[tokio::test]
async fn remote_original_is_served_in_chunks() {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static STORE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().clear();
    let app = axum::Router::new().fallback(|req: Request<Body>| async move {
        use axum::response::IntoResponse;
        let store = STORE.get_or_init(|| Mutex::new(HashMap::new()));
        let path = req.uri().path().to_string();
        if *req.method() != axum::http::Method::GET {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let Some(bytes) = store.lock().unwrap().get(&path).cloned() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let span = req
            .headers()
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)));
        if let Some((start, end)) = span {
            return (StatusCode::PARTIAL_CONTENT, bytes[start..=end].to_vec()).into_response();
        }
        (StatusCode::OK, bytes).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Remote"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{album}");
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "remote.png", 8).await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (blob_id, key): (String, String) = sqlx::query_as(
        "SELECT i.original_blob_id, l.object_key FROM images i
         JOIN media_locations l ON l.blob_id = i.original_blob_id
         WHERE i.id = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let bytes = std::fs::read(dir.path().join("media").join(&key)).unwrap();
    STORE.get().unwrap().lock().unwrap().insert("/gallery/remote.png".into(), bytes.clone());
    let cfg = serde_json::json!({
        "kind": "s3",
        "region": "us-east-1",
        "bucket": "gallery",
        "prefix": "",
        "endpoint": format!("http://{addr}"),
        "access_key": "test-key",
        "secret_key": "test-secret"
    })
    .to_string();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('be-remote', 's3', ?, 1, ?)",
    )
    .bind(&cfg)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE media_locations SET backend_id = 'be-remote', object_key = 'remote.png' WHERE blob_id = ?",
    )
    .bind(&blob_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE storage_route SET active_backend_id = 'be-remote' WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let url = format!("/api/v1/albums/{album_id}/images/{image_id}/original-view");
    let (status, _, body) = h.call(Request::get(&url).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
    let (status, headers, partial) = h
        .call(
            Request::get(&url)
                .header(header::RANGE, "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(partial, bytes[..4]);
    assert_eq!(
        headers.get(header::CONTENT_RANGE).unwrap().to_str().unwrap(),
        format!("bytes 0-3/{}", bytes.len())
    );
}

#[tokio::test]
async fn original_range_is_partial_and_anonymous_etag_stays_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Range"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "range.png", 8).await;
    let url = format!("/api/v1/albums/{album_id}/images/{image_id}/original-view");
    let (status, headers, full) = h
        .call(Request::get(&url).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::ACCEPT_RANGES).unwrap(), "bytes");
    let etag = headers.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let (status, headers, part) = h
        .call(
            Request::get(&url)
                .header(header::RANGE, "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part, full[..4]);
    assert_eq!(
        headers.get(header::CONTENT_RANGE).unwrap().to_str().unwrap(),
        format!("bytes 0-3/{}", full.len())
    );
    let (status, _, suffix) = h
        .call(
            Request::get(&url)
                .header(header::RANGE, "bytes=-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(suffix, full[full.len() - 4..]);
    let (status, headers, empty) = h
        .call(
            Request::get(&url)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(empty.is_empty());
    assert_eq!(headers.get(header::ETAG).unwrap().to_str().unwrap(), etag);
    let (status, _, ignored) = h
        .call(
            Request::get(&url)
                .header(header::RANGE, "bytes=0-3")
                .header(header::IF_RANGE, "\"other\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ignored, full);
    let hidden = h
        .app
        .clone()
        .oneshot(
            Request::get(&url)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archive_becomes_stale_when_album_content_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Packed"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap();
    let (status, created) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/archives"))
                .header("x-gallery-client", "desktop")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let archive_id = created["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status, view) = h
            .json(
                Request::get(format!("/api/v1/archives/{archive_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        if view["state"] == "ready" {
            ready = true;
            break;
        }
    }
    assert!(ready, "archive should become ready");
    let (status, current) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, reorder_body) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/items/reorder"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_id": image_id,
                        "image_ids": [image_id],
                        "expected_revision": current["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reorder_body}");
    let (status, view) = h
        .json(
            Request::get(format!("/api/v1/archives/{archive_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["state"], "stale");
    assert!(view["body_url"].is_null());
    let (status, body) = h
        .json(
            Request::get(format!("/api/v1/archives/{archive_id}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "ARCHIVE_STALE");
    let (status, again) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/archives"))
                .header("x-gallery-client", "desktop")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_ne!(again["id"], archive_id);
}

#[tokio::test]
async fn selected_download_replays_claims_and_expiry_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Pick"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap();
    let body = serde_json::json!({"image_ids": [image_id, "missing-image"]}).to_string();
    let select = |key: &str, payload: String| {
        Request::post(format!("/api/v1/albums/{album_id}/downloads/selected"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from(payload))
            .unwrap()
    };
    let (status, first) = h.json(select("pick-1", body.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["items"][0]["ok"], true);
    assert_eq!(first["items"][1]["ok"], false);
    assert_eq!(first["items"][1]["code"], "NOT_FOUND");
    let (status, second) = h.json(select("pick-1", body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["items"][0]["claim_id"], second["items"][0]["claim_id"]);
    let (status, conflict) = h
        .json(select(
            "pick-1",
            serde_json::json!({"image_ids": ["missing-image"]}).to_string(),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "IDEMPOTENCY_CONFLICT");
    let claim_id = first["items"][0]["claim_id"].as_str().unwrap();
    let (status, _, bytes) = h
        .call(
            Request::get(format!("/api/v1/downloads/claims/{claim_id}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query("UPDATE download_claims SET expires_at = 1 WHERE id = ?")
        .bind(claim_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, expired) = h
        .json(
            Request::get(format!("/api/v1/downloads/claims/{claim_id}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(expired["code"], "CLAIM_EXPIRED");
    let (status, created_user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let _ = created_user;
    login_as(&mut h, "bea", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, _, _) = h
        .call(
            Request::get(format!("/api/v1/downloads/claims/{claim_id}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cancel_upload_keeps_committed_items_and_replays_terminal_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Cancel"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "items": [
                            {"client_item_id": "kept", "name": "kept.png", "size": png.len()},
                            {"client_item_id": "drop", "name": "drop.png", "size": png.len()}
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap().to_string();
    let kept = batch["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["client_item_id"] == "kept")
        .unwrap()["upload_item_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{kept}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();
    let (status, canceled) = h
        .json(
            Request::delete(format!("/api/v1/uploads/{batch_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(canceled["state"], "partial");
    let items = canceled["items"].as_array().unwrap();
    let kept_item = items.iter().find(|item| item["client_item_id"] == "kept").unwrap();
    let dropped = items.iter().find(|item| item["client_item_id"] == "drop").unwrap();
    assert_eq!(kept_item["state"], "committed");
    assert_eq!(kept_item["image_id"], image_id);
    assert_eq!(dropped["state"], "canceled");
    let (status, again) = h
        .json(
            Request::delete(format!("/api/v1/uploads/{batch_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["state"], "partial");
    assert_eq!(
        again["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["client_item_id"] == "kept")
            .unwrap()["image_id"],
        image_id
    );
}

#[tokio::test]
async fn trash_restore_and_purge_follow_lifecycle_revision() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Bin"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();
    let trash = |revision: i64| {
        Request::post(format!("/api/v1/images/{image_id}/trash"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"expected_lifecycle_revision": revision}).to_string(),
            ))
            .unwrap()
    };
    let (status, missing) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/trash"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(missing["code"], "LIFECYCLE_REQUIRED");
    let (status, conflict) = h.json(trash(99)).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(conflict["code"], "LIFECYCLE_CONFLICT");
    let (status, trashed) = h.json(trash(1)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trashed["state"], "trashed");
    let lifecycle = trashed["lifecycle_revision"].as_i64().unwrap();
    let (status, listed) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed["items"].as_array().unwrap().is_empty());
    let (status, bin) = h
        .json(Request::get("/api/v1/trash").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bin["items"][0]["id"], image_id);
    let (status, restored) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/restore"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"expected_lifecycle_revision": lifecycle}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restored["state"], "active");
    let (status, listed) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"][0]["id"], image_id);
    let lifecycle = restored["lifecycle_revision"].as_i64().unwrap();
    let (status, trashed) = h.json(trash(lifecycle)).await;
    assert_eq!(status, StatusCode::OK);
    let lifecycle = trashed["lifecycle_revision"].as_i64().unwrap();
    let (status, _) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/purge"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"expected_lifecycle_revision": 1}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let (status, purged) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/purge"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"expected_lifecycle_revision": lifecycle}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(purged["job_id"].as_str().unwrap().len() > 8);
    let mut gone = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status, bin) = h
            .json(Request::get("/api/v1/trash").body(Body::empty()).unwrap())
            .await;
        assert_eq!(status, StatusCode::OK);
        if bin["items"].as_array().unwrap().is_empty() {
            gone = true;
            break;
        }
    }
    assert!(gone, "purged image should leave the trash");
}

#[tokio::test]
async fn cancel_only_the_named_job_and_keep_terminal_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE role = 'owner'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, actor_id, available_at, created_at, updated_at)
         VALUES('job-live', 'reconcile', 'dedupe-live', '{}', '{}', 'queued', ?, ?, ?, ?),
                ('job-done', 'reconcile', 'dedupe-done', '{}', '{}', 'succeeded', ?, ?, ?, ?),
                ('job-other', 'reconcile', 'dedupe-other', '{}', '{}', 'queued', ?, ?, ?, ?)",
    )
    .bind(&owner.0)
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .bind(&owner.0)
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .bind(&owner.0)
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, created_at, updated_at)
         VALUES('sess-job', ?, '{}', 'active', ?, ?)",
    )
    .bind(&owner.0)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at)
         VALUES('turn-job', 'sess-job', 1, 'running', ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let (status, _) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    login_as(&mut h, "bea", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, _, _) = h
        .call(
            Request::post("/api/v1/jobs/job-live/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    h.login().await;
    let (status, canceled) = h
        .json(
            Request::post("/api/v1/jobs/job-live/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(canceled["state"], "canceled");
    let (status, again) = h
        .json(
            Request::post("/api/v1/jobs/job-live/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["state"], "canceled");
    let (status, done) = h
        .json(
            Request::post("/api/v1/jobs/job-done/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["state"], "succeeded");
    let (status, other) = h
        .json(Request::get("/api/v1/jobs/job-other").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(other["state"], "queued");
    let turn: (String,) = sqlx::query_as("SELECT state FROM agent_turns WHERE id = 'turn-job'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(turn.0, "running");
    let (status, retried) = h
        .json(
            Request::post("/api/v1/jobs/job-live/retry")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(retried["state"], "queued");
    let (status, kept) = h
        .json(
            Request::post("/api/v1/jobs/job-done/retry")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kept["state"], "succeeded");
}

#[tokio::test]
async fn conversion_output_belongs_to_the_session_and_expires() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Convert"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(16, 16, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();
    let (status, created) = h
        .json(
            Request::post("/api/v1/conversions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_id": image_id,
                        "spec": {
                            "spec_id": "tiny",
                            "fit": "cover",
                            "width": 8,
                            "height": 8,
                            "format": "png",
                            "png_compression": "fast"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let conversion_id = created["id"].as_str().unwrap().to_string();
    let mut output_id = None;
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status, view) = h
            .json(
                Request::get(format!("/api/v1/conversions/{conversion_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        if view["state"] == "ready" {
            output_id = view["output_id"].as_str().map(str::to_string);
            break;
        }
    }
    let output_id = output_id.expect("conversion should become ready");
    let (status, _, bytes) = h
        .call(
            Request::get(format!(
                "/api/v1/conversions/{conversion_id}/outputs/{output_id}/body"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!bytes.is_empty());
    let (status, _) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    login_as(&mut h, "bea", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, _, _) = h
        .call(
            Request::get(format!("/api/v1/conversions/{conversion_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, hidden) = h
        .json(
            Request::post("/api/v1/conversions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_id": image_id,
                        "spec": {
                            "spec_id": "tiny",
                            "fit": "cover",
                            "width": 8,
                            "height": 8,
                            "format": "png",
                            "png_compression": "fast"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(hidden["code"], "NOT_FOUND");
    h.login().await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query("UPDATE conversions SET expires_at = 1 WHERE id = ?")
        .bind(&conversion_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, view) = h
        .json(
            Request::get(format!("/api/v1/conversions/{conversion_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["state"], "expired");
    assert!(view["output_id"].is_null());
    let (status, gone) = h
        .json(
            Request::get(format!(
                "/api/v1/conversions/{conversion_id}/outputs/{output_id}/body"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(gone["code"], "CONVERSION_EXPIRED");
}

#[tokio::test]
async fn human_caption_checks_revision_and_edit_permission() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Notes"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO captions(image_id, source, text, updated_at) VALUES(?, 'model', '模型描述', ?)",
    )
    .bind(&image_id)
    .bind(gallery::db::now_ms())
    .execute(&pool)
    .await
    .unwrap();
    let (status, missing) = h
        .json(
            Request::patch(format!("/api/v1/images/{image_id}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"手写"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(missing["code"], "REVISION_REQUIRED");
    let (status, conflict) = h
        .json(
            Request::patch(format!("/api/v1/images/{image_id}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"手写","expected_revision":99}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(conflict["code"], "REVISION_CONFLICT");
    let (status, saved) = h
        .json(
            Request::patch(format!("/api/v1/images/{image_id}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"手写描述","expected_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["source"], "human");
    assert_eq!(saved["revision"], 2);
    let (status, detail) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["caption"]["source"], "human");
    assert_eq!(detail["caption"]["text"], "手写描述");
    let (status, created_user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let collab_id = created_user["id"].as_str().unwrap().to_string();
    login_as(&mut h, "bea", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, _, _) = h
        .call(
            Request::patch(format!("/api/v1/images/{image_id}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"越权","expected_revision":2}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    h.login().await;
    let (status, current) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": current["revision"],
                        "grants": [{"actor_id": collab_id, "browse": true, "edit": true}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, edited) = h
        .json(
            Request::patch(format!("/api/v1/images/{image_id}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"协作者描述","expected_revision":2}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["text"], "协作者描述");
    assert_eq!(edited["source"], "human");
}

#[tokio::test]
async fn human_caption_checks_revision_and_beats_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Notes"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();
    let caption = |text: &str, revision: i64| {
        Request::patch(format!("/api/v1/images/{image_id}/caption"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"text": text, "expected_revision": revision}).to_string(),
            ))
            .unwrap()
    };
    let (status, missing) = h
        .json(
            Request::patch(format!("/api/v1/images/{image_id}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":"no revision"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(missing["code"], "REVISION_REQUIRED");
    let (status, conflict) = h.json(caption("wrong", 9)).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(conflict["code"], "REVISION_CONFLICT");
    let (status, saved) = h.json(caption("人手写的", 1)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["source"], "human");
    assert_eq!(saved["revision"], 2);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO captions(image_id, source, text, updated_at) VALUES(?, 'model', '模型写的', ?)",
    )
    .bind(&image_id)
    .bind(gallery::db::now_ms())
    .execute(&pool)
    .await
    .unwrap();
    let (status, detail) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["caption"]["source"], "human");
    assert_eq!(detail["caption"]["text"], "人手写的");
    let (status, user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"bea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let collab_id = user["id"].as_str().unwrap().to_string();
    login_as(&mut h, "bea", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, _) = h.json(caption("别人写的", 2)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    h.login().await;
    let (status, current) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": current["revision"],
                        "grants": [{"actor_id": collab_id, "browse": true, "edit": true}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    login_as(&mut h, "bea", "collab-pass-13").await;
    let (status, edited) = h.json(caption("协作者写的", 2)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["text"], "协作者写的");
    assert_eq!(edited["source"], "human");
}

#[tokio::test]
async fn profile_change_enqueues_variant_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Specs"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(32, 24, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, _) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, headers, body) = h
        .call(Request::get("/api/v1/variant-profiles/current").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let profile: Value = serde_json::from_slice(&body).unwrap();
    let (status, same) = h
        .json(
            Request::put("/api/v1/variant-profiles/current")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, &etag)
                .body(Body::from(profile["specs"].to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(same["revision"], profile["revision"]);
    assert!(same["job_ids"].as_array().unwrap().is_empty());
    let (status, missing) = h
        .json(
            Request::put("/api/v1/variant-profiles/current")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(profile["specs"].to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(missing["code"], "REVISION_REQUIRED");
    let mut specs = profile["specs"].as_array().unwrap().clone();
    specs[0]["width"] = serde_json::json!(111);
    let (status, stale) = h
        .json(
            Request::put("/api/v1/variant-profiles/current")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "\"99\"")
                .body(Body::from(serde_json::to_string(&specs).unwrap()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale["code"], "REVISION_CONFLICT");
    let (status, changed) = h
        .json(
            Request::put("/api/v1/variant-profiles/current")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, &etag)
                .body(Body::from(serde_json::to_string(&specs).unwrap()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(changed["revision"], profile["revision"].as_i64().unwrap() + 1);
    assert!(!changed["job_ids"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn move_requires_both_revisions_and_deleted_album_can_return() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let make = |name: &str| {
        Request::post("/api/v1/albums")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
            .unwrap()
    };
    let (status, source) = h.json(make("From")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, target) = h.json(make("To")).await;
    assert_eq!(status, StatusCode::CREATED);
    let source_id = source["id"].as_str().unwrap().to_string();
    let target_id = target["id"].as_str().unwrap().to_string();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap().to_string();
    let (status, source_now) = h
        .json(
            Request::get(format!("/api/v1/albums/{source_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, target_now) = h
        .json(
            Request::get(format!("/api/v1/albums/{target_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, missing) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/items/move"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "target_album_id": target_id,
                        "expected_revision": source_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(missing["code"], "REVISION_REQUIRED");
    let (status, stale) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/items/move"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "target_album_id": target_id,
                        "expected_revision": source_now["revision"],
                        "target_expected_revision": 99
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale["code"], "REVISION_CONFLICT");
    let (status, _moved) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/items/move"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "target_album_id": target_id,
                        "expected_revision": source_now["revision"],
                        "target_expected_revision": target_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, source_images) = h
        .json(
            Request::get(format!("/api/v1/albums/{source_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(source_images["items"].as_array().unwrap().is_empty());
    let (status, target_images) = h
        .json(
            Request::get(format!("/api/v1/albums/{target_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(target_images["items"][0]["id"], image_id);
    let keeper = png_bytes(6, 6, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"b","name":"keep.png","size": keeper.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, kept) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(keeper))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let keeper_id = kept["image_id"].as_str().unwrap().to_string();
    let (status, source_now) = h
        .json(
            Request::get(format!("/api/v1/albums/{source_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, archive) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/archives"))
                .header("x-gallery-client", "desktop")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(status == StatusCode::ACCEPTED || status == StatusCode::OK, "{archive}");
    let archive_id = archive["id"].as_str().unwrap().to_string();
    let (status, created_user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"cara","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let _ = created_user;
    login_as(&mut h, "cara", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "cara", "collab-pass-13").await;
    let (status, denied) = h
        .json(
            Request::delete(format!("/api/v1/albums/{source_id}"))
                .header(header::IF_MATCH, format!("\"{}\"", source_now["revision"]))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(denied["code"], "NOT_FOUND");
    h.login().await;
    let (status, _) = h
        .json(
            Request::delete(format!("/api/v1/albums/{source_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let source_revision = source_now["revision"].as_i64().unwrap();
    let (status, _, _) = h
        .call(
            Request::delete(format!("/api/v1/albums/{source_id}"))
                .header(header::IF_MATCH, format!("\"{source_revision}\""))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, archived) = h
        .json(
            Request::get(format!("/api/v1/archives/{archive_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(archived["state"], "stale");
    let (status, stale_body) = h
        .json(
            Request::get(format!("/api/v1/archives/{archive_id}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale_body["code"], "ARCHIVE_STALE");
    let (status, again) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/archives"))
                .header("x-gallery-client", "desktop")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{again}");
    let (status, library) = h
        .json(
            Request::get("/api/v1/library/images?unassigned=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(library["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|image| image["id"] == keeper_id));
    assert!(library["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|image| image["id"] != image_id));
    let (status, target_images) = h
        .json(
            Request::get(format!("/api/v1/albums/{target_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(target_images["items"][0]["id"], image_id);
    let (status, listed) = h
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|album| album["id"] != source_id));
    let (status, hidden) = h
        .json(
            Request::get(format!("/api/v1/albums/{source_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let deleted_revision = hidden["revision"].as_i64().unwrap();
    let (status, restored) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/restore"))
                .header(header::IF_MATCH, format!("\"{deleted_revision}\""))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restored["id"], source_id);
    let (status, listed) = h
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|album| album["id"] == source_id));
    let (status, source_images) = h
        .json(
            Request::get(format!("/api/v1/albums/{source_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(source_images["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|image| image["id"] == keeper_id));
    let (status, library) = h
        .json(
            Request::get("/api/v1/library/images?unassigned=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(library["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|image| image["id"] != keeper_id));
}

#[tokio::test]
async fn search_rebuild_uses_the_index_and_falls_back_when_it_breaks() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Lighthouse Keep","description":"north shore"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let (status, rebuild) = h
        .json(Request::post("/api/v1/admin/search/rebuild").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job_id = rebuild["job_id"].as_str().unwrap();
    let mut job_state = String::new();
    for _ in 0..50 {
        let (status, job) = h
            .json(Request::get(format!("/api/v1/jobs/{job_id}")).body(Body::empty()).unwrap())
            .await;
        assert_eq!(status, StatusCode::OK);
        job_state = job["state"].as_str().unwrap().to_string();
        if job_state == "succeeded" || job_state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(job_state, "succeeded");
    let (status, found) = h
        .json(Request::get("/api/v1/search?q=lighthouse").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(found["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == album_id));
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query("DROP TABLE search_fts").execute(&pool).await.unwrap();
    let (status, fallback) = h
        .json(Request::get("/api/v1/search?q=lighthouse").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(fallback["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == album_id));
}

#[tokio::test]
async fn search_respects_permission_kind_and_literal_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, private_album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"RedBridge","description":"north pond"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let private_id = private_album["id"].as_str().unwrap().to_string();
    let (status, public_album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"BlueLake","description":"open water"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let public_id = public_album["id"].as_str().unwrap().to_string();
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{public_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "visibility": "public",
                        "expected_revision": public_album["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let red = upload_png(&h, &private_id, "red-fish.png", 8).await;
    let blue = upload_png(&h, &public_id, "blue-bird.png", 9).await;
    let loose = upload_png(&h, &private_id, "loose-leaf.png", 10).await;
    let (status, private_now) = h
        .json(
            Request::get(format!("/api/v1/albums/{private_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .json(
            Request::post(format!("/api/v1/albums/{private_id}/items/remove"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [loose],
                        "expected_revision": private_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, caption) = h
        .json(
            Request::patch(format!("/api/v1/images/{red}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"text": "赤桥倒影", "expected_revision": 1}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{caption}");
    let (status, hits) = h
        .json(Request::get("/api/v1/search?q=dbr").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert!(hits["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|album| album["id"] == private_id));
    let (status, caption_hits) = h
        .json(
            Request::get("/api/v1/search?q=%E8%B5%A4%E6%A1%A5%E5%80%92")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{caption_hits}");
    assert!(caption_hits["items"]["images"]
        .as_array()
        .unwrap()
        .iter()
        .any(|image| image["id"] == red));
    let (status, short_hits) = h
        .json(
            Request::get("/api/v1/search?q=%E6%A1%A5")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(short_hits["items"]["images"]
        .as_array()
        .unwrap()
        .iter()
        .any(|image| image["id"] == red));
    let (status, literal) = h
        .json(Request::get("/api/v1/search?q=%25").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(literal["items"]["albums"].as_array().unwrap().is_empty());
    assert!(literal["items"]["images"].as_array().unwrap().is_empty());
    let (status, albums_only) = h
        .json(
            Request::get("/api/v1/search?q=blue&kind=album")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(albums_only["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|album| album["id"] == public_id));
    assert!(albums_only["items"]["images"].as_array().unwrap().is_empty());
    let (status, in_public) = h
        .json(
            Request::get(format!("/api/v1/search?q=bird&album_id={public_id}&kind=image"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(in_public["items"]["images"][0]["id"], blue);
    assert!(in_public["items"]["albums"].as_array().unwrap().is_empty());
    let (status, library_hit) = h
        .json(Request::get("/api/v1/search?q=loose").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK, "{library_hit}");
    assert!(
        library_hit["items"]["images"]
            .as_array()
            .unwrap()
            .iter()
            .any(|image| image["id"] == loose && image["album_id"].is_null()),
        "{library_hit}"
    );
    let (status, page) = h
        .json(Request::get("/api/v1/search?q=e&limit=1").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let cursor = page["next_cursor"].as_str().unwrap();
    let (status, next_page) = h
        .json(
            Request::get(format!(
                "/api/v1/search?q=e&limit=1&cursor={}",
                urlencoding_cursor(cursor)
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(page["items"], next_page["items"]);
    let (status, created_user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"ned","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let _ = created_user;
    login_as(&mut h, "ned", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "ned", "collab-pass-13").await;
    let (status, hidden) = h
        .json(Request::get("/api/v1/search?q=RedBridge").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(hidden["items"]["albums"].as_array().unwrap().is_empty());
    assert!(hidden["items"]["images"].as_array().unwrap().is_empty());
    let (status, public_hit) = h
        .json(Request::get("/api/v1/search?q=BlueLake").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(public_hit["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|album| album["id"] == public_id));
    let (status, leaked) = h
        .json(Request::get("/api/v1/search?q=loose").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(leaked["items"]["images"].as_array().unwrap().is_empty());
    h.cookie.clear();
    h.csrf.clear();
    let (status, anon) = h
        .json(Request::get("/api/v1/search?q=RedBridge").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(anon["items"]["albums"].as_array().unwrap().is_empty());
    let (status, anon_public) = h
        .json(Request::get("/api/v1/search?q=BlueLake").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(anon_public["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|album| album["id"] == public_id));
}

#[tokio::test]
async fn reconcile_marks_orphans_before_deleting_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let media_root = dir.path().join("media");
    let orphan = media_root.join("stray").join("orphan.bin");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"not-indexed").unwrap();
    let kept = media_root.join("media").join("ab").join("kept.bin");
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    std::fs::write(&kept, b"indexed-bytes").unwrap();
    let cfg = {
        let mut cfg = gallery::config::Config::from_env().unwrap();
        cfg.data_dir = dir.path().to_path_buf();
        cfg
    };
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let sha = gallery::ids::sha256_hex(b"indexed-bytes");
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let backend = state.media().await.unwrap().backend_id;
    gallery::storage::record_blob(
        &mut tx,
        &sha,
        b"indexed-bytes".len() as i64,
        "application/octet-stream",
        "original",
        &backend,
        "media/ab/kept.bin",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for _ in 0..40 {
        let running: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM jobs WHERE kind = 'reconcile' AND state IN ('queued', 'running', 'retry_wait')",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if running.0 == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (status, queued) = h
        .json(
            Request::post("/api/v1/admin/reconcile")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    let job_id = queued["job_id"].as_str().unwrap();
    let mut done = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status, job) = h
            .json(
                Request::get(format!("/api/v1/jobs/{job_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        if job["state"] == "succeeded" {
            done = true;
            break;
        }
    }
    assert!(done, "reconcile job should finish");
    assert!(orphan.is_file(), "first pass only marks the orphan");
    assert!(kept.is_file());
    let marked: (String,) = sqlx::query_as(
        "SELECT state FROM storage_reclaim WHERE object_key = 'stray/orphan.bin'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(marked.0, "marked");
    let kept_mark: Option<(String,)> = sqlx::query_as(
        "SELECT state FROM storage_reclaim WHERE object_key = 'media/ab/kept.bin'",
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap();
    assert!(kept_mark.is_none());
    sqlx::query("UPDATE storage_reclaim SET marked_at = 0 WHERE object_key = 'stray/orphan.bin'")
        .execute(&state.pool)
        .await
        .unwrap();
    let report = gallery::storage::reconcile::run(&state).await.unwrap();
    assert_eq!(report["deleted"], 1);
    assert!(!orphan.is_file());
    assert!(kept.is_file());
    std::fs::remove_file(&kept).unwrap();
    let report = gallery::storage::reconcile::run(&state).await.unwrap();
    assert!(report["missing"].as_i64().unwrap() >= 1);
    let location: (String,) = sqlx::query_as(
        "SELECT state FROM media_locations WHERE object_key = 'media/ab/kept.bin'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(location.0, "verified");
}

#[tokio::test]
async fn reorder_rebalances_when_neighbor_keys_are_adjacent() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let album = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum { name: "Order".into(), description: None },
    )
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    for (index, image_id) in ["img-a", "img-b", "img-c"].iter().enumerate() {
        let sha = format!("{index:064x}");
        sqlx::query(
            "INSERT INTO media_blobs(id, sha256, size, mime, state, kind, created_at) VALUES(?, ?, 4, 'image/png', 'verified', 'original', ?)",
        )
        .bind(format!("blob-{image_id}"))
        .bind(&sha)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
             VALUES(?, ?, ?, ?, 8, 8, 'image/png', ?, ?)",
        )
        .bind(image_id)
        .bind(&sha)
        .bind(format!("blob-{image_id}"))
        .bind(format!("{image_id}.png"))
        .bind(index as i64 + 1)
        .bind(now)
        .execute(&state.pool)
        .await
        .unwrap();
    }
    gallery::album::add_images(
        &state.pool,
        &actor,
        &album.id,
        &["img-a".into(), "img-b".into(), "img-c".into()],
        Some(album.revision),
    )
    .await
    .unwrap();
    for (image_id, key) in [("img-a", 10), ("img-b", 11), ("img-c", 40)] {
        sqlx::query("UPDATE album_images SET sort_key = ? WHERE album_id = ? AND image_id = ?")
            .bind(key)
            .bind(&album.id)
            .bind(image_id)
            .execute(&state.pool)
            .await
            .unwrap();
    }
    let current = gallery::album::get(&state.pool, Some(&actor), &album.id).await.unwrap();
    gallery::album::reorder(
        &state.pool,
        &actor,
        &album.id,
        "img-c",
        Some("img-b"),
        Some("img-a"),
        Some(current.revision),
        None,
    )
    .await
    .unwrap();
    let listed = gallery::album::list_images(&state.pool, Some(&actor), &album.id, "manual", None, 10, None)
        .await
        .unwrap();
    let ids: Vec<_> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["img-a".to_string(), "img-c".to_string(), "img-b".to_string()]);
    let keys = sqlx::query_as::<_, (String, i64)>(
        "SELECT image_id, sort_key FROM album_images WHERE album_id = ? ORDER BY sort_key, image_id",
    )
    .bind(&album.id)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert!(keys[1].1 > keys[0].1 + 1);
    assert!(keys[2].1 > keys[1].1 + 1);
    assert_eq!(keys[0].0, "img-a");
    assert_eq!(keys[1].0, "img-c");
    assert_eq!(keys[2].0, "img-b");
}

#[tokio::test]
async fn disabling_a_collaborator_checks_revision_and_drops_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, owner) = h
        .json(Request::get("/api/v1/session").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let owner_id = owner["actor"]["id"].as_str().unwrap().to_string();
    let (status, created) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"ida","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["revision"], 1);
    let user_id = created["id"].as_str().unwrap().to_string();
    login_as(&mut h, "ida", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "ida", "collab-pass-13").await;
    let collab_cookie = h.cookie.clone();
    let collab_csrf = h.csrf.clone();
    h.login().await;
    let (status, missing) = h
        .json(
            Request::patch(format!("/api/v1/admin/users/{user_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"status":"disabled"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(missing["code"], "REVISION_REQUIRED");
    let (status, stale) = h
        .json(
            Request::patch(format!("/api/v1/admin/users/{user_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"status":"disabled","expected_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale["code"], "REVISION_CONFLICT");
    let (status, owner_block) = h
        .json(
            Request::patch(format!("/api/v1/admin/users/{owner_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"status":"disabled","expected_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(owner_block["code"], "ACCOUNT_OWNER");
    let (status, disabled) = h
        .json(
            Request::patch(format!("/api/v1/admin/users/{user_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"status":"disabled","expected_revision":2}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disabled["revision"], 3);
    h.cookie = collab_cookie;
    h.csrf = collab_csrf;
    let (status, session) = h
        .json(Request::get("/api/v1/session").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(session["actor"].is_null());
    h.login().await;
    let (status, enabled) = h
        .json(
            Request::patch(format!("/api/v1/admin/users/{user_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"status":"active","expected_revision":3}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enabled["status"], "active");
    let (status, _, _) = h
        .call(
            Request::post(format!("/api/v1/admin/users/{user_id}/reset-password"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"password":"collab-pass-14"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let (status, _, _) = h
        .call(
            Request::post(format!("/api/v1/admin/users/{user_id}/reset-password"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"password":"collab-pass-14","expected_revision":4}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "ida", "collab-pass-14").await;
    let (status, gated) = h
        .json(Request::get("/api/v1/albums").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(gated["code"], "MUST_CHANGE_PASSWORD");
}

#[tokio::test]
async fn terminal_jobs_emit_a_finished_event() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, actor_id, state, available_at, lease_token, lease_until, created_at, updated_at)
         VALUES('job-ok', 'generate_variant', 'dedupe-ok', '{}', '{}', 'actor-1', 'running', ?, 'lease-ok', ?, ?, ?)",
    )
    .bind(now)
    .bind(now + 60_000)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-wait', 'build_archive', 'dedupe-wait', '{}', '{}', 'queued', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    assert!(gallery::jobs::commit_result(&pool, "job-ok", "lease-ok", true, None).await.unwrap());
    assert!(!gallery::jobs::commit_result(&pool, "job-ok", "lease-ok", true, None).await.unwrap());
    gallery::jobs::cancel(&pool, "job-wait").await.unwrap();
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT json_extract(payload_json, '$.id'), json_extract(payload_json, '$.state')
         FROM events WHERE type = 'job.finished' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![("job-ok".into(), "succeeded".into()), ("job-wait".into(), "canceled".into())]);
    let notice: (String,) = sqlx::query_as(
        "SELECT payload_json FROM events WHERE type = 'job.finished' AND json_extract(payload_json, '$.id') = 'job-ok'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&notice.0).unwrap();
    assert_eq!(parsed["status_url"], "/api/v1/jobs/job-ok");
    assert_eq!(parsed["progress"]["done"], parsed["progress"]["total"]);
    assert!(!notice.0.contains("payload"));
}

#[tokio::test]
async fn job_progress_event_keeps_only_the_reference_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, actor_id, state, progress_done, progress_total, available_at, lease_token, lease_until, created_at, updated_at)
         VALUES('job-step', 'build_archive', 'dedupe-step', '{\"path\":\"/tmp/secret.png\"}', '{}', 'actor-1', 'running', 0, 4, ?, 'lease-step', ?, ?, ?)",
    )
    .bind(now)
    .bind(now + 60_000)
    .bind(now)
    .bind(now - 2_000)
    .execute(&pool)
    .await
    .unwrap();
    assert!(gallery::jobs::report_progress(&pool, "job-step", "lease-step", 2, 4).await.unwrap());
    assert!(!gallery::jobs::report_progress(&pool, "job-step", "lease-step", 3, 4).await.unwrap());
    let payload: (String,) = sqlx::query_as(
        "SELECT payload_json FROM events WHERE type = 'job.progress' AND resource_id = 'job-step'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&payload.0).unwrap();
    assert_eq!(parsed["id"], "job-step");
    assert_eq!(parsed["status_url"], "/api/v1/jobs/job-step");
    assert_eq!(parsed["progress"]["done"], 2);
    assert_eq!(parsed["progress"]["total"], 4);
    assert!(!payload.0.contains("/tmp/secret"));
    assert!(!payload.0.contains("sk-"));
}

#[tokio::test]
async fn shutdown_stops_claims_then_cancels_or_releases_leases() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    for (id, state, kind) in [
        ("job-run", "running", "backup"),
        ("job-stop", "cancel_requested", "generate_variant"),
        ("job-next", "queued", "generate_variant"),
    ] {
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, priority, available_at, lease_token, lease_until, created_at, updated_at)
             VALUES(?, ?, ?, '{}', '{}', ?, 100, ?, 'lease', ?, ?, ?)",
        )
        .bind(id)
        .bind(kind)
        .bind(format!("dedupe-{id}"))
        .bind(state)
        .bind(now)
        .bind(now + 60_000)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    gallery::jobs::begin_drain(&pool).await.unwrap();
    assert!(gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().is_none());
    let queued: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-next'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(queued.0, "queued");
    gallery::jobs::shutdown_workers(&pool, std::time::Duration::ZERO).await.unwrap();
    let stopped: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-stop'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stopped.0, "canceled");
    let released: (String, Option<String>, i64) =
        sqlx::query_as("SELECT state, lease_token, lease_until FROM jobs WHERE id = 'job-run'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(released.0, "running");
    assert!(released.1.is_none());
    assert!(released.2 < now + 60_000);
    assert!(released.2 <= gallery::db::now_ms());
    assert!(gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().is_none());
    gallery::jobs::resume_workers(&pool).await.unwrap();
    let claimed = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(claimed.0, "job-next");
    let recovered: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-run'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(recovered.0, "retry_wait");
}

#[tokio::test]
async fn retry_delay_is_jittered_and_summary_counts_execution() {
    assert_eq!(gallery::jobs::next_retry_delay(1, 0, None), 30_000);
    assert_eq!(gallery::jobs::next_retry_delay(1, 9_000, None), 35_000);
    assert_eq!(gallery::jobs::next_retry_delay(2, 1_000, None), 121_000);
    assert_eq!(gallery::jobs::next_retry_delay(2, 0, Some(600_000)), 300_000);
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, progress_done, progress_total, available_at, lease_token, lease_until, created_at, updated_at)
         VALUES('job-run', 'generate_variant', 'dedupe-run', '{}', '{}', 'running', 0, 4, ?, 'lease-1', ?, ?, ?)",
    )
    .bind(now)
    .bind(now + 60_000)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    assert!(!gallery::jobs::report_progress(&pool, "job-run", "lease-1", 1, 4).await.unwrap());
    sqlx::query("UPDATE jobs SET updated_at = ? WHERE id = 'job-run'")
        .bind(now - 2_000)
        .execute(&pool)
        .await
        .unwrap();
    assert!(gallery::jobs::report_progress(&pool, "job-run", "lease-1", 2, 4).await.unwrap());
    let progress: (i64,) = sqlx::query_as("SELECT progress_done FROM jobs WHERE id = 'job-run'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(progress.0, 2);
    for (id, state) in [("job-ok", "succeeded"), ("job-bad", "failed"), ("job-wait", "retry_wait"), ("job-ready", "queued")] {
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
             VALUES(?, 'backup', ?, '{}', '{}', ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(format!("dedupe-{id}"))
        .bind(state)
        .bind(if id == "job-wait" { now + 60_000 } else { now })
        .bind(now - 5_000)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO job_attempts(job_id, attempt_no, worker_id, started_at, finished_at, result)
         VALUES('job-ok', 2, 'worker', ?, ?, 'succeeded')",
    )
    .bind(now - 4_000)
    .bind(now - 1_000)
    .execute(&pool)
    .await
    .unwrap();
    let summary = gallery::jobs::summary(&pool).await.unwrap();
    assert_eq!(summary["ready_count"], 1);
    assert_eq!(summary["failure_rate"], 0.5);
    assert_eq!(summary["retries"], 1);
    assert_eq!(summary["execution_ms"], 3_000);
}

#[tokio::test]
async fn clock_jump_pauses_claims_until_leases_match_database_time() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::jobs::observe_clock(&pool, 0, 0).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, lease_until, created_at, updated_at)
         VALUES('job-running', 'backup', 'dedupe-running', '{}', '{}', 'running', ?, 60000, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, priority, available_at, created_at, updated_at)
         VALUES('job-queued', 'generate_variant', 'dedupe-queued', '{}', '{}', 'queued', 100, ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    assert!(gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().is_none());
    let still: (String, i64) = sqlx::query_as("SELECT state, lease_until FROM jobs WHERE id = 'job-running'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still.0, "running");
    assert_eq!(still.1, 60_000);
    let skew = gallery::jobs::recover_clock(&pool, now, 1_000).await.unwrap();
    assert!(skew > 30_000);
    let shifted: (i64,) = sqlx::query_as("SELECT lease_until FROM jobs WHERE id = 'job-running'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(shifted.0, 60_000 + skew);
    assert!(shifted.0 > now);
    let claimed = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(claimed.0, "job-queued");
}

#[tokio::test]
async fn kind_slots_and_actor_fairness_shape_the_next_claim() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    for (id, kind, actor, state, priority) in [
        ("run-a", "generate_variant", "actor-a", "running", 100),
        ("run-b", "generate_variant", "actor-a", "running", 100),
        ("wait-variant", "generate_variant", "actor-a", "queued", 100),
        ("wait-archive", "build_archive", "actor-b", "queued", 100),
    ] {
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, actor_id, state, priority, available_at, lease_until, created_at, updated_at)
             VALUES(?, ?, ?, '{}', '{}', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(kind)
        .bind(format!("dedupe-{id}"))
        .bind(actor)
        .bind(state)
        .bind(priority)
        .bind(now)
        .bind(now + 60_000)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    let first = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(first.0, "wait-archive");
    sqlx::query("UPDATE jobs SET state = 'succeeded', lease_token = NULL WHERE id IN ('run-a', 'run-b', 'wait-archive')")
        .execute(&pool)
        .await
        .unwrap();
    for (id, actor) in [("hold-a", "actor-a"), ("next-a", "actor-a"), ("next-b", "actor-b")] {
        let state = if id == "hold-a" { "running" } else { "queued" };
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, actor_id, state, priority, available_at, lease_until, created_at, updated_at)
             VALUES(?, 'generate_variant', ?, '{}', '{}', ?, ?, 100, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(format!("dedupe-{id}"))
        .bind(actor)
        .bind(state)
        .bind(now)
        .bind(now + 60_000)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    let fair = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(fair.0, "next-b");
}

#[tokio::test]
async fn backlog_lets_thumbnails_pass_scans_and_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('queue_backlog_suppress', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (id, kind, priority) in [
        ("job-scan", "scan_plan", 1),
        ("job-rebuild", "generate_variant", 10),
        ("job-thumb", "generate_variant", 100),
    ] {
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, priority, available_at, created_at, updated_at)
             VALUES(?, ?, ?, '{}', '{}', 'queued', ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(kind)
        .bind(format!("dedupe-{id}"))
        .bind(priority)
        .bind(now)
        .bind(now - 5_000)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, lease_until, created_at, updated_at)
         VALUES('job-expired-lease', 'backup', 'dedupe-expired-lease', '{}', '{}', 'running', ?, ?, ?, ?)",
    )
    .bind(now)
    .bind(now - 1_000)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let summary = gallery::jobs::summary(&pool).await.unwrap();
    assert_eq!(summary["suppressed"], true);
    assert!(summary["oldest_pending_seconds"].as_i64().unwrap() >= 5);
    assert_eq!(summary["expired_leases_total"], 1);
    let claimed = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(claimed.0, "job-thumb");
    let scan: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-scan'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let rebuild: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE id = 'job-rebuild'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(scan.0, "queued");
    assert_eq!(rebuild.0, "queued");
    sqlx::query("UPDATE schema_app SET value = '100' WHERE key = 'queue_backlog_suppress'")
        .execute(&pool)
        .await
        .unwrap();
    let next = gallery::jobs::claim_next(&pool, "worker-test").await.unwrap().unwrap();
    assert_eq!(next.0, "job-rebuild");
}

#[tokio::test]
async fn full_queue_rejects_new_work_but_keeps_upload_reservations() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('queue_soft_cap', '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-fill', 'reconcile', 'dedupe-fill', '{}', '{}', 'queued', ?, ?, ?)",
    )
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Queue"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let (status, full) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"items":[{"client_item_id":"a","name":"a.png","size":8}]}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(full["code"], "QUEUE_FULL");
    assert_eq!(full["retryable"], true);
    sqlx::query("DELETE FROM jobs WHERE id = 'job-fill'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE schema_app SET value = '8' WHERE key = 'queue_soft_cap'")
        .execute(&pool)
        .await
        .unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{batch}");
    let batch_id = batch["id"].as_str().unwrap();
    let reserved: (i64,) = sqlx::query_as("SELECT slots FROM job_reservations WHERE id = ?")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reserved.0, 4);
    sqlx::query("UPDATE schema_app SET value = '0' WHERE key = 'queue_soft_cap'")
        .execute(&pool)
        .await
        .unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{commit}");
    assert!(!commit["job_ids"].as_array().unwrap().is_empty());
    let variants: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE kind = 'generate_variant'")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(variants.0 >= 1);
}

#[tokio::test]
async fn backup_retention_keeps_seven_days_and_four_weeks() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-23T12:00:00Z")
        .unwrap()
        .timestamp_millis();
    let day = 86_400_000_i64;
    for offset in 0..=8 {
        let id = format!("bak-{offset}");
        let at = now - offset * day;
        sqlx::query(
            "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at) VALUES(?, 'succeeded', ?, ?, ?)",
        )
        .bind(&id)
        .bind(at)
        .bind(at)
        .bind(at)
        .execute(&state.pool)
        .await
        .unwrap();
    }
    for offset in [21_i64, 28, 35] {
        let id = format!("bak-week-{offset}");
        let at = now - offset * day;
        sqlx::query(
            "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at) VALUES(?, 'succeeded', ?, ?, ?)",
        )
        .bind(&id)
        .bind(at)
        .bind(at)
        .bind(at)
        .execute(&state.pool)
        .await
        .unwrap();
    }
    let ancient_dir = dir.path().join("backups").join("bak-ancient");
    let dropped_dir = dir.path().join("backups").join("bak-7");
    std::fs::create_dir_all(&ancient_dir).unwrap();
    std::fs::create_dir_all(&dropped_dir).unwrap();
    std::fs::write(ancient_dir.join("manifest.json"), b"{}").unwrap();
    std::fs::write(dropped_dir.join("manifest.json"), b"{}").unwrap();
    let ancient_at = now - 60 * day;
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at) VALUES('bak-ancient', 'succeeded', ?, ?, ?)",
    )
    .bind(ancient_at)
    .bind(ancient_at)
    .bind(ancient_at)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, snapshot_at, created_at, updated_at) VALUES('bak-failed', 'failed', ?, ?, ?)",
    )
    .bind(ancient_at)
    .bind(ancient_at)
    .bind(ancient_at)
    .execute(&state.pool)
    .await
    .unwrap();
    let expired = gallery::backup::prune_retention(&state.pool, &dir.path().join("backups"), now)
        .await
        .unwrap();
    assert!(expired.iter().any(|id| id == "bak-ancient"));
    assert!(expired.iter().any(|id| id == "bak-7"));
    assert!(expired.iter().any(|id| id == "bak-week-35"));
    assert!(!expired.iter().any(|id| id == "bak-0" || id == "bak-8" || id == "bak-week-21"));
    assert!(!ancient_dir.exists());
    assert!(!dropped_dir.exists());
    let newest: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-0'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(newest.0, "succeeded");
    let weekly: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-8'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(weekly.0, "succeeded");
    let failed: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-failed'")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(failed.0, "failed");
}

#[tokio::test]
async fn adding_an_image_requires_visibility_and_public_browse_hides_originals() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, hidden) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"HiddenCopy"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let hidden_id = hidden["id"].as_str().unwrap().to_string();
    let (status, shared) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"SharedCopy"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let shared_id = shared["id"].as_str().unwrap().to_string();
    let image_id = upload_png(&h, &hidden_id, "secret.png", 12).await;
    let (status, created_user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"lea","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let collab_id = created_user["id"].as_str().unwrap().to_string();
    let (status, shared_now) = h
        .json(
            Request::get(format!("/api/v1/albums/{shared_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{shared_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": shared_now["revision"],
                        "grants": [{
                            "actor_id": collab_id,
                            "browse": true,
                            "upload": true,
                            "edit": false
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    login_as(&mut h, "lea", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "lea", "collab-pass-13").await;
    let (status, shared_for_lea) = h
        .json(
            Request::get(format!("/api/v1/albums/{shared_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, denied) = h
        .json(
            Request::post(format!("/api/v1/albums/{shared_id}/items/add"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "expected_revision": shared_for_lea["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(denied["items"][0]["code"], "NOT_FOUND");
    h.login().await;
    let (status, shared_now) = h
        .json(
            Request::get(format!("/api/v1/albums/{shared_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, added) = h
        .json(
            Request::post(format!("/api/v1/albums/{shared_id}/items/add"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "expected_revision": shared_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(added["items"][0]["ok"], true);
    let sort_before: (i64,) = {
        let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
            .await
            .unwrap();
        sqlx::query_as("SELECT sort_key FROM album_images WHERE album_id = ? AND image_id = ?")
            .bind(&shared_id)
            .bind(&image_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let (status, again) = h
        .json(
            Request::post(format!("/api/v1/albums/{shared_id}/items/add"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "expected_revision": added["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["items"][0]["code"], "ALREADY_EXISTS");
    assert_eq!(again["revision"], added["revision"]);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let sort_after: (i64,) = sqlx::query_as(
        "SELECT sort_key FROM album_images WHERE album_id = ? AND image_id = ?",
    )
    .bind(&shared_id)
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sort_before.0, sort_after.0);
    let (status, hidden_now) = h
        .json(
            Request::get(format!("/api/v1/albums/{hidden_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{hidden_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "visibility": "public",
                        "expected_revision": hidden_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    h.cookie.clear();
    h.csrf.clear();
    let (status, images) = h
        .json(
            Request::get(format!("/api/v1/albums/{hidden_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(images["items"][0]["id"], image_id);
    let (status, _) = h
        .json(
            Request::get(format!(
                "/api/v1/albums/{hidden_id}/images/{image_id}/original-download"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .json(
            Request::get(format!("/api/v1/albums/{shared_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn previous_variant_stays_readable_for_seven_days() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Versions"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let image_id = upload_png(&h, &album_id, "v.png", 20).await;
    let old_thumb = wait_thumb(&h, &album_id).await;
    let (status, headers, body) = h
        .call(Request::get("/api/v1/variant-profiles/current").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let profile: Value = serde_json::from_slice(&body).unwrap();
    let mut specs = profile["specs"].as_array().unwrap().clone();
    specs[0]["width"] = serde_json::json!(120);
    let (status, changed) = h
        .json(
            Request::put("/api/v1/variant-profiles/current")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, &etag)
                .body(Body::from(serde_json::to_string(&specs).unwrap()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{changed}");
    let new_thumb = wait_thumb_change(&h, &album_id, &old_thumb).await;
    let (status, _, bytes) = h
        .call(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/variants/{old_thumb}/body"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!bytes.is_empty());
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let retain: (Option<i64>,) =
        sqlx::query_as("SELECT retain_until FROM image_variants WHERE id = ?")
            .bind(&old_thumb)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(retain.0.is_some());
    sqlx::query("UPDATE image_variants SET retain_until = 0 WHERE id = ?")
        .bind(&old_thumb)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = h
        .json(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/variants/{old_thumb}/body"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let state_name: (String,) = sqlx::query_as("SELECT state FROM image_variants WHERE id = ?")
        .bind(&old_thumb)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_name.0, "superseded");
    let (status, _, _) = h
        .call(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/variants/{new_thumb}/body"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn expired_archive_stays_while_a_download_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media"))
        .await
        .unwrap();
    let actor: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let owner = gallery::auth::Actor {
        id: actor.0.clone(),
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let album = gallery::album::create(
        &pool,
        &owner,
        gallery::album::CreateAlbum { name: "Zip".into(), description: None },
    )
    .await
    .unwrap();
    let bytes = b"zip-body";
    storage.put_bytes("archives/pkg.zip", bytes, "application/zip").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(bytes),
        bytes.len() as i64,
        "application/zip",
        "archive",
        &storage.backend_id,
        "archives/pkg.zip",
    )
    .await
    .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO archive_packages(id, album_id, actor_id, content_revision, acl_revision, policy_epoch, archive_state, blob_id, expires_at, created_at)
         VALUES('pkg-1', ?, ?, 1, 1, 1, 'ready', ?, ?, ?)",
    )
    .bind(&album.id)
    .bind(&actor.0)
    .bind(&blob_id)
    .bind(now + 60_000)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(gallery::download::hold_archive(&pool, "pkg-1", &actor.0).await.unwrap());
    sqlx::query("UPDATE archive_packages SET expires_at = ? WHERE id = 'pkg-1'")
        .bind(now - 1_000)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(gallery::download::expire_archives(&pool, &storage).await.unwrap(), 0);
    assert!(dir.path().join("media").join("archives").join("pkg.zip").is_file());
    {
        let _hold = gallery::download::ArchiveHold::new(pool.clone(), "pkg-1", actor.0.clone());
        assert_eq!(gallery::download::expire_archives(&pool, &storage).await.unwrap(), 0);
    }
    let mut released = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let left: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM archive_reads WHERE archive_id = 'pkg-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        if left.0 == 0 {
            released = true;
            break;
        }
    }
    assert!(released);
    assert!(!gallery::download::hold_archive(&pool, "pkg-1", &actor.0).await.unwrap());
    assert_eq!(gallery::download::expire_archives(&pool, &storage).await.unwrap(), 1);
    let state: (String,) = sqlx::query_as("SELECT archive_state FROM archive_packages WHERE id = 'pkg-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state.0, "expired");
    assert!(!dir.path().join("media").join("archives").join("pkg.zip").is_file());
}

#[tokio::test]
async fn archive_stales_on_policy_epoch_and_fails_when_media_is_missing() {
    assert!(gallery::download::archive_fits(Some(10), 10));
    assert!(!gallery::download::archive_fits(Some(9), 10));
    assert!(gallery::download::archive_fits(None, 10));
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Packed","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "pack.png", 8).await;
    let (status, refused) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/archives"))
                .header(header::USER_AGENT, "Mozilla/5.0 (iPhone)")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["code"], "ARCHIVE_CLIENT");
    let (status, arch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/archives"))
                .header("x-gallery-client", "desktop")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let archive_id = arch["id"].as_str().unwrap().to_string();
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, st) = h
            .json(Request::get(format!("/api/v1/archives/{archive_id}")).body(Body::empty()).unwrap())
            .await;
        if st["state"] == "ready" {
            ready = true;
            break;
        }
    }
    assert!(ready);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query("UPDATE schema_app SET value = '2' WHERE key = 'policy_epoch'")
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = h
        .json(Request::get(format!("/api/v1/archives/{archive_id}/body")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "ARCHIVE_STALE");
    let (key,): (String,) = sqlx::query_as(
        "SELECT l.object_key FROM media_locations l
         JOIN images i ON i.original_blob_id = l.blob_id
         WHERE i.id = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    std::fs::remove_file(dir.path().join("media").join(key)).unwrap();
    let (status, again) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/archives"))
                .header("x-gallery-client", "desktop")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let missing_id = again["id"].as_str().unwrap();
    let mut failed = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, st) = h
            .json(Request::get(format!("/api/v1/archives/{missing_id}")).body(Body::empty()).unwrap())
            .await;
        if st["state"] == "failed" {
            assert_eq!(st["missing"], serde_json::json!([image_id]));
            failed = true;
            break;
        }
    }
    assert!(failed);
    let code: (Option<String>,) = sqlx::query_as(
        "SELECT failure_code FROM jobs WHERE id = (SELECT job_id FROM archive_packages WHERE id = ?)",
    )
    .bind(missing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(code.0.as_deref(), Some("MEDIA_MISSING"));
}

#[tokio::test]
async fn variant_job_stops_on_decode_permission_and_config() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Codes","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "plain.png", 8).await;
    wait_thumb(&h, album_id).await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, status, revision, created_at)
         VALUES('actor-revoked', 'revoked', 'x', 'collaborator', 'active', 1, ?)",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    async fn enqueue(pool: &sqlx::SqlitePool, id: &str, image_id: &str, spec: &str, actor: Option<&str>) {
        let now = gallery::db::now_ms();
        let payload = serde_json::json!({
            "kind": "generate_variant",
            "payload_version": 1,
            "scope": {"image_id": image_id},
            "target": {"spec_id": spec, "profile_revision": 1},
            "actor_id": actor,
        });
        sqlx::query(
            "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, actor_id, state, priority, available_at, created_at, updated_at)
             VALUES(?, 'generate_variant', ?, ?, ?, ?, 'queued', 100, ?, ?, ?)",
        )
        .bind(id)
        .bind(format!("dedupe-{id}"))
        .bind(payload.to_string())
        .bind(format!(r#"{{"image_id":"{image_id}"}}"#))
        .bind(actor)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }
    async fn wait_code(pool: &sqlx::SqlitePool, id: &str) -> (String, Option<String>, i64) {
        for _ in 0..80 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let row = sqlx::query_as::<_, (String, Option<String>, i64)>(
                "SELECT state, failure_code, attempt_no FROM jobs WHERE id = ?",
            )
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
            if row.0 == "failed" || row.0 == "succeeded" {
                return row;
            }
        }
        panic!("job {id} did not finish");
    }
    enqueue(&pool, "job-config", &image_id, "no-such-spec", None).await;
    let config = wait_code(&pool, "job-config").await;
    assert_eq!(config.0, "failed");
    assert_eq!(config.1.as_deref(), Some("CONFIG"));
    assert_eq!(config.2, 1);
    enqueue(&pool, "job-revoked", &image_id, "thumb", Some("actor-revoked")).await;
    let revoked = wait_code(&pool, "job-revoked").await;
    assert_eq!(revoked.1.as_deref(), Some("PERMISSION_REVOKED"));
    assert_eq!(revoked.2, 1);
    sqlx::query("UPDATE images SET state = 'trashed' WHERE id = ?")
        .bind(&image_id)
        .execute(&pool)
        .await
        .unwrap();
    enqueue(&pool, "job-gone", &image_id, "thumb", None).await;
    let gone = wait_code(&pool, "job-gone").await;
    assert_eq!(gone.1.as_deref(), Some("IMAGE_UNAVAILABLE"));
    sqlx::query("UPDATE images SET state = 'active' WHERE id = ?")
        .bind(&image_id)
        .execute(&pool)
        .await
        .unwrap();
    let (key,): (String,) = sqlx::query_as(
        "SELECT l.object_key FROM media_locations l
         JOIN images i ON i.original_blob_id = l.blob_id
         WHERE i.id = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    std::fs::write(dir.path().join("media").join(&key), b"not-an-image").unwrap();
    enqueue(&pool, "job-bytes", &image_id, "thumb", None).await;
    let bytes = wait_code(&pool, "job-bytes").await;
    assert_eq!(bytes.0, "failed");
    assert_eq!(bytes.1.as_deref(), Some("UNDECODABLE"));
    assert_eq!(bytes.2, 1);
}

async fn wait_thumb(h: &Harness, album_id: &str) -> String {
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (_, images) = h
            .json(
                Request::get(format!("/api/v1/albums/{album_id}/images"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        if let Some(id) = images["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["variants"].as_array())
            .and_then(|variants| variants.iter().find(|variant| variant["spec_id"] == "thumb"))
            .and_then(|variant| variant["id"].as_str())
        {
            return id.to_string();
        }
    }
    panic!("thumb was not published");
}

async fn wait_thumb_change(h: &Harness, album_id: &str, previous: &str) -> String {
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let current = wait_thumb_once(h, album_id).await;
        if current.as_deref() != Some(previous) {
            return current.expect("replacement thumb");
        }
    }
    panic!("replacement thumb was not published");
}

async fn wait_thumb_once(h: &Harness, album_id: &str) -> Option<String> {
    let (_, images) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    images["items"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["variants"].as_array())
        .and_then(|variants| variants.iter().find(|variant| variant["spec_id"] == "thumb"))
        .and_then(|variant| variant["id"].as_str())
        .map(str::to_string)
}

#[tokio::test]
async fn derivative_applies_jpeg_orientation_and_keeps_original_bytes() {
    let bytes = jpeg_with_orientation(6);
    assert_eq!(gallery::processing::jpeg_orientation(&bytes), 6);
    let probe = gallery::processing::probe_bytes(&bytes).unwrap();
    assert_eq!(probe.orientation, 6);
    assert_eq!((probe.width, probe.height), (8, 4));
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("turned.jpg");
    let output = dir.path().join("turned.png");
    std::fs::write(&input, &bytes).unwrap();
    let plan = gallery::processing::CodecPlan {
        plan_id: "orient".into(),
        ops: vec![gallery::processing::CodecOp {
            input_path: input,
            output_path: output.clone(),
            spec_id: "thumb".into(),
            format: "png".into(),
            width: None,
            height: None,
            long_edge: Some(100),
            fit: "contain".into(),
        }],
    };
    let outputs = gallery::processing::run_plan(&plan).unwrap();
    assert_eq!((outputs[0].width, outputs[0].height), (4, 8));
    let png = std::fs::read(&output).unwrap();
    assert!(!png.windows(4).any(|window| window == b"Exif"));
    assert_eq!(gallery::processing::jpeg_orientation(&png), 1);
}

fn jpeg_with_orientation(orientation: u8) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(8, 4, image::Rgb([20, 40, 60]));
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&[
        b'I',
        b'I',
        42,
        0,
        8,
        0,
        0,
        0,
        1,
        0,
        0x12,
        0x01,
        3,
        0,
        1,
        0,
        0,
        0,
        orientation,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ]);
    let len = (payload.len() + 2) as u16;
    let mut segment = vec![0xFF, 0xE1, (len >> 8) as u8, (len & 0xFF) as u8];
    segment.extend(payload);
    let mut out = Vec::with_capacity(jpeg.len() + segment.len());
    out.extend_from_slice(&jpeg[..2]);
    out.extend_from_slice(&segment);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[tokio::test]
async fn original_download_uses_utf8_filename() {
    let disposition = gallery::http_util::content_disposition("赤桥.png");
    assert!(disposition.contains("filename*=UTF-8''%E8%B5%A4%E6%A1%A5.png"));
    assert!(!disposition.contains('/'));
    assert!(gallery::http_util::content_disposition("CON.png").contains("filename=\"_CON.png\""));
    assert!(!gallery::http_util::content_disposition("a/b\\c.png").contains("filename=\"a/b"));
    assert!(gallery::http_util::content_disposition("a\nb.png").contains("filename=\"ab.png\""));
    assert!(gallery::http_util::content_disposition("...").contains("filename=\"download\""));

    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Names"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "赤桥.png", 8).await;
    let (status, headers, _) = h
        .call(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/original-download"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.starts_with("attachment;"));
    assert!(disposition.contains("filename*=UTF-8''%E8%B5%A4%E6%A1%A5.png"));
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
}

#[tokio::test]
async fn paused_job_kind_stays_queued_until_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, bad) = h
        .json(
            Request::post("/api/v1/admin/job-pauses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"kind":"shell","reason":"no"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad["code"], "JOB_PAUSE");
    let (status, _, _) = h
        .call(
            Request::post("/api/v1/admin/job-pauses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"kind":"generate_variant","reason":"codec backlog"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-paused-kind', 'generate_variant', 'dedupe-paused-kind', '{}', '{}', 'queued', ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let job = gallery::jobs::get(&pool, "job-paused-kind").await.unwrap().unwrap();
    assert_eq!(job.state, "queued");
    let (status, summary) = h
        .json(Request::get("/api/v1/admin/jobs/summary").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(summary["pauses"]
        .as_array()
        .unwrap()
        .iter()
        .any(|pause| pause["kind"] == "generate_variant" && pause["reason"] == "codec backlog"));
    assert!(summary["backlog"]["queued"].as_i64().unwrap() >= 1);
    assert!(!summary.to_string().contains("secret"));
    let (status, _, _) = h
        .call(
            Request::delete("/api/v1/admin/job-pauses/generate_variant")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mut released = false;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let job = gallery::jobs::get(&pool, "job-paused-kind").await.unwrap().unwrap();
        if job.state != "queued" {
            released = true;
            break;
        }
    }
    assert!(released, "resumed kind should be claimed");
}

#[tokio::test]
async fn ready_checks_sqlite_and_media_without_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let app = gallery::scheduler::router(state.clone());
    let res = app
        .clone()
        .oneshot(Request::get("/health/live").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let ready: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(ready["sqlite"], "ok");
    assert_eq!(ready["media"], "ok");
    assert_eq!(ready["maintenance"], false);
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(!text.contains(dir.path().to_str().unwrap()));
    assert!(!text.contains("secret"));
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&state.pool)
        .await
        .unwrap();
    let res = app
        .clone()
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ready: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(ready["maintenance"], true);
    let media = dir.path().join("media");
    let parked = dir.path().join("media-parked");
    std::fs::rename(&media, &parked).unwrap();
    let res = app
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let down: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(down["media"], "unavailable");
    assert!(!down.to_string().contains(dir.path().to_str().unwrap()));
    std::fs::rename(&parked, &media).unwrap();
}

#[tokio::test]
async fn settings_limits_are_revisioned_and_storage_stays_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, settings) = h
        .json(Request::get("/api/v1/admin/settings").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settings["storage"]["writable"], false);
    assert_eq!(settings["ai"]["writable"], false);
    assert_eq!(settings["limits"]["max_upload_items"], 200);
    assert!(!settings.to_string().contains("secret"));
    assert!(!settings.to_string().contains(dir.path().to_str().unwrap()));
    let (status, readonly) = h
        .json(
            Request::patch("/api/v1/admin/settings/storage")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_revision":1,"max_upload_items":2}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(readonly["code"], "SETTINGS_READONLY");
    let (status, missing) = h
        .json(
            Request::patch("/api/v1/admin/settings/limits")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"max_upload_items":2}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(missing["code"], "REVISION_REQUIRED");
    let (status, updated) = h
        .json(
            Request::patch("/api/v1/admin/settings/limits")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_revision":1,"max_upload_items":2,"max_download_selection":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["revision"], 2);
    assert_eq!(updated["limits"]["source"], "sqlite");
    let (status, caps) = h
        .json(Request::get("/api/v1/capabilities").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(caps["max_upload_items"], 2);
    assert_eq!(caps["max_download_selection"], 1);
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Limits"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let (status, too_many) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"items":[{"client_item_id":"a","name":"a.png","size":8},{"client_item_id":"b","name":"b.png","size":8},{"client_item_id":"c","name":"c.png","size":8}]}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(too_many["code"], "BATCH_SIZE");
    let (status, created_user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"kay","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let _ = created_user;
    login_as(&mut h, "kay", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "kay", "collab-pass-13").await;
    let (status, denied) = h
        .json(
            Request::patch("/api/v1/admin/settings/limits")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_revision":2,"max_upload_items":3}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "FORBIDDEN");
}

#[tokio::test]
async fn canceling_a_scan_keeps_caption_work_other_triggers_still_need() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0.clone(),
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let now = gallery::db::now_ms();
    let model = gallery::vision::model_key();
    let prompt = gallery::vision::PROMPT_REVISION;
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    for (id, label, seq) in [("img-current", "current", 1_i64), ("img-stale", "stale", 2), ("img-missing", "missing", 3)] {
        let sha = gallery::ids::sha256_hex(label.as_bytes());
        let blob = gallery::storage::record_blob(
            &mut tx,
            &sha,
            label.len() as i64,
            "application/octet-stream",
            "original",
            &storage.backend_id,
            &format!("media/{id}.bin"),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
             VALUES(?, ?, ?, ?, 8, 8, 'application/octet-stream', 'active', ?, ?)",
        )
        .bind(id)
        .bind(&sha)
        .bind(&blob)
        .bind(format!("{label}.bin"))
        .bind(seq)
        .bind(now)
        .execute(&mut *tx)
        .await
        .unwrap();
        if id != "img-missing" {
            let recorded = if id == "img-current" { sha } else { "old-source".into() };
            sqlx::query(
                "INSERT INTO captions(image_id, source, model_key, prompt_revision, input_sha, source_sha256, text, review_state, updated_at)
                 VALUES(?, 'model', ?, ?, 'input', ?, 'kept', 'published', ?)",
            )
            .bind(id)
            .bind(&model)
            .bind(prompt)
            .bind(&recorded)
            .bind(now)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
    }
    tx.commit().await.unwrap();
    let due = gallery::ai::next_scan_images(&pool, 0, 10, None, None, &model, prompt)
        .await
        .unwrap();
    let due_ids: Vec<_> = due.into_iter().map(|(id, _, _)| id).collect();
    assert_eq!(due_ids, vec!["img-stale".to_string(), "img-missing".to_string()]);

    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    gallery::ai::enqueue_caption(&mut tx, &actor.id, "img-shared", "sha-shared", None, true)
        .await
        .unwrap();
    gallery::ai::enqueue_caption(&mut tx, &actor.id, "img-shared", "sha-shared", Some("scan-1"), false)
        .await
        .unwrap();
    gallery::ai::enqueue_caption(&mut tx, &actor.id, "img-shared", "sha-shared", Some("scan-2"), false)
        .await
        .unwrap();
    gallery::ai::enqueue_caption(&mut tx, &actor.id, "img-only", "sha-only", Some("scan-1"), false)
        .await
        .unwrap();
    let plan = gallery::jobs::JobPayload {
        kind: "scan_plan".into(),
        payload_version: 1,
        scope: serde_json::json!({"scan_id": "scan-1"}),
        target: serde_json::json!({}),
        actor_id: Some(actor.id.clone()),
    };
    gallery::jobs::enqueue(&mut tx, "scan_plan", "scan_plan:scan-1:0", &plan, Some(&actor.id), 2)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO ai_scans(id, actor_id, scope_json, state, cursor_seq, created_at, updated_at)
         VALUES('scan-1', ?, '{}', 'running', 0, ?, ?)",
    )
    .bind(&actor.id)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let canceled = gallery::ai::cancel_scan(&pool, &actor, "scan-1").await.unwrap();
    assert_eq!(canceled["state"], "canceled");
    let shared: (String, String) = sqlx::query_as(
        "SELECT state, scope_json FROM jobs WHERE dedupe_key = ?",
    )
    .bind(format!("caption:img-shared:sha-shared:{model}:{prompt}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(shared.0, "queued");
    let scope: serde_json::Value = serde_json::from_str(&shared.1).unwrap();
    assert_eq!(scope["upload"], true);
    assert_eq!(scope["scan_ids"], serde_json::json!(["scan-2"]));
    let only: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE dedupe_key = ?")
        .bind(format!("caption:img-only:sha-only:{model}:{prompt}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(only.0, "canceled");
    let parent: (String,) = sqlx::query_as("SELECT state FROM jobs WHERE kind = 'scan_plan'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(parent.0, "canceled");
}

#[tokio::test]
async fn caption_reuses_a_published_derivative_under_1024() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let original = png_bytes(40, 12, false);
    let mobile = png_bytes(18, 9, false);
    let thumb = png_bytes(8, 8, false);
    let tablet = png_bytes(14, 6, false);
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let original_blob = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(&original),
        original.len() as i64,
        "image/png",
        "original",
        &storage.backend_id,
        "media/original.png",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
         VALUES('img-vision', ?, ?, 'original.png', 40, 12, 'image/png', 'active', 1, ?)",
    )
    .bind(gallery::ids::sha256_hex(&original))
    .bind(&original_blob)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    for (spec, bytes, width, height) in [
        ("thumb", &thumb, 256_i64, 256),
        ("mobile", &mobile, 960, 640),
        ("tablet", &tablet, 1440, 900),
    ] {
        let key = format!("media/{spec}.png");
        let blob = gallery::storage::record_blob(
            &mut tx,
            &gallery::ids::sha256_hex(bytes),
            bytes.len() as i64,
            "image/png",
            "variant",
            &storage.backend_id,
            &key,
        )
        .await
        .unwrap();
        let variant_id = format!("var-{spec}");
        sqlx::query(
            "INSERT INTO image_variants(id, image_id, profile_revision, spec_id, blob_id, state, width, height)
             VALUES(?, 'img-vision', 1, ?, ?, 'published', ?, ?)",
        )
        .bind(&variant_id)
        .bind(spec)
        .bind(&blob)
        .bind(width)
        .bind(height)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO variant_heads(image_id, spec_id, variant_id, published_at)
             VALUES('img-vision', ?, ?, ?)",
        )
        .bind(spec)
        .bind(&variant_id)
        .bind(gallery::db::now_ms())
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    storage.put_bytes("media/original.png", &original, "image/png").await.unwrap();
    storage.put_bytes("media/thumb.png", &thumb, "image/png").await.unwrap();
    storage.put_bytes("media/mobile.png", &mobile, "image/png").await.unwrap();
    storage.put_bytes("media/tablet.png", &tablet, "image/png").await.unwrap();
    let reused = gallery::vision::load_inference_input(&pool, &storage, "img-vision", &original)
        .await
        .unwrap();
    assert_eq!(reused.reused_spec.as_deref(), Some("mobile"));
    assert_eq!(reused.bytes, mobile);
    assert_eq!(reused.mime, "image/png");
    sqlx::query("DELETE FROM variant_heads WHERE image_id = 'img-vision' AND spec_id IN ('thumb', 'mobile')")
        .execute(&pool)
        .await
        .unwrap();
    let generated = gallery::vision::load_inference_input(&pool, &storage, "img-vision", &original)
        .await
        .unwrap();
    assert!(generated.reused_spec.is_none());
    let (png, sha) = gallery::vision::inference_png(&original).unwrap();
    assert_eq!(generated.bytes, png);
    assert_eq!(generated.sha, sha);
}

#[tokio::test]
async fn withdrawn_image_is_not_attached_by_a_later_upload() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Hold","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let image_id = upload_png(&h, &album_id, "same.png", 8).await;
    let (status, trashed) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/trash"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_lifecycle_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trashed["state"], "trashed");
    let (status, before) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let revision = before["revision"].clone();
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (jobs_before,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE json_extract(scope_json, '$.image_id') = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"again","name":"same.png","size":png.len()}]}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let batch_id = batch["id"].as_str().unwrap();
    let (status, body) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "IMAGE_UNAVAILABLE");
    let (status, after) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["revision"], revision);
    let (jobs_after,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE json_extract(scope_json, '$.image_id') = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs_after, jobs_before);
    let (item_state, code): (String, Option<String>) = sqlx::query_as(
        "SELECT state, failure_code FROM upload_items WHERE id = ?",
    )
    .bind(item_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(item_state, "failed");
    assert_eq!(code.as_deref(), Some("IMAGE_UNAVAILABLE"));
}

#[tokio::test]
async fn missing_object_is_marked_and_a_blip_stays_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Reads","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap().to_string();
    let image_id = upload_png(&h, &album_id, "read.png", 400).await;
    let thumb_id = wait_thumb(&h, &album_id).await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (original_key,): (String,) = sqlx::query_as(
        "SELECT l.object_key FROM media_locations l
         JOIN images i ON i.original_blob_id = l.blob_id
         WHERE i.id = ? AND l.state = 'verified'",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let original_path = dir.path().join("media").join(&original_key);
    std::fs::remove_file(&original_path).unwrap();
    std::fs::create_dir(&original_path).unwrap();
    let original_url = format!("/api/v1/albums/{album_id}/images/{image_id}/original-download");
    let (status, blip) = h
        .json(Request::get(&original_url).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(blip["code"], "STORAGE_UNAVAILABLE");
    assert_eq!(blip["retryable"], true);
    assert_eq!(blip["retry_after_ms"], 5000);
    let (still,): (String,) = sqlx::query_as(
        "SELECT state FROM media_locations WHERE object_key = ?",
    )
    .bind(&original_key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(still, "verified");
    std::fs::remove_dir(&original_path).unwrap();
    let (status, missing) = h
        .json(Request::get(&original_url).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["code"], "MEDIA_MISSING");
    assert_eq!(missing["retryable"], false);
    let (failed,): (String,) = sqlx::query_as(
        "SELECT state FROM media_locations WHERE object_key = ?",
    )
    .bind(&original_key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failed, "failed");
    let (thumb_key, thumb_blob): (String, String) = sqlx::query_as(
        "SELECT l.object_key, l.blob_id
         FROM variant_heads h
         JOIN image_variants v ON v.id = h.variant_id
         JOIN media_locations l ON l.blob_id = v.blob_id
         WHERE h.image_id = ? AND h.spec_id = 'thumb' AND l.state = 'verified'",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    std::fs::remove_file(dir.path().join("media").join(&thumb_key)).unwrap();
    let (status, thumb_missing) = h
        .json(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/variants/{thumb_id}/body"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(thumb_missing["code"], "MEDIA_MISSING");
    let (thumb_state,): (String,) = sqlx::query_as(
        "SELECT state FROM media_locations WHERE blob_id = ?",
    )
    .bind(&thumb_blob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(thumb_state, "failed");
    let (rebuilds,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE dedupe_key = ?",
    )
    .bind(format!("rebuild_variant:{thumb_blob}:thumb"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rebuilds, 1);
}

#[tokio::test]
async fn signed_complete_finishes_a_backup_whose_status_write_was_lost() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let keys = dir.path().join("keys");
    let root = dir.path().join("backups");
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"pinned"),
        6,
        "application/octet-stream",
        "original",
        &storage.backend_id,
        "media/pinned.bin",
    )
    .await
    .unwrap();
    for id in ["bak-signed", "bak-bad"] {
        sqlx::query(
            "INSERT INTO backup_runs(id, state, manifest_digest, created_at, updated_at)
             VALUES(?, 'running', ?, ?, ?)",
        )
        .bind(id)
        .bind(gallery::ids::sha256_hex(id.as_bytes()))
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO backup_pins(backup_id, blob_id) VALUES(?, ?)")
            .bind(id)
            .bind(&blob)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    for id in ["bak-signed", "bak-bad"] {
        let dest = root.join(id);
        std::fs::create_dir_all(&dest).unwrap();
        let body = id.as_bytes();
        std::fs::write(dest.join("manifest.json"), body).unwrap();
        let digest = gallery::ids::sha256_hex(body);
        sqlx::query("UPDATE backup_runs SET manifest_digest = ? WHERE id = ?")
            .bind(&digest)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        if id == "bak-signed" {
            let signed = gallery::backup::signed_complete(&keys, id, &digest).unwrap();
            std::fs::write(dest.join("COMPLETE"), signed).unwrap();
        } else {
            std::fs::write(dest.join("COMPLETE"), format!("{digest}\nnot-a-signature")).unwrap();
        }
    }
    let finished = gallery::backup::finish_signed_backups(&pool, &root, &keys)
        .await
        .unwrap();
    assert_eq!(finished, vec!["bak-signed".to_string()]);
    let signed_state: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-signed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(signed_state.0, "succeeded");
    let signed_pins: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-signed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(signed_pins.0, 0);
    let bad_state: (String,) = sqlx::query_as("SELECT state FROM backup_runs WHERE id = 'bak-bad'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(bad_state.0, "running");
    let bad_pins: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_pins WHERE backup_id = 'bak-bad'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(bad_pins.0, 1);
}

#[tokio::test]
async fn upload_reuses_a_reserved_original_after_the_insert_was_lost() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Reserve","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"once","name":"once.png","size":png.len()}]}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let sha = gallery::ids::sha256_hex(&png);
    let image_id = "img-reserved";
    let key = gallery::storage::original_key(image_id, &sha, "image/png");
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let until = gallery::db::now_ms() + 60_000;
    sqlx::query(
        "INSERT INTO original_reservations(source_sha256, image_id, lease_token, lease_until, planned_object_key)
         VALUES(?, ?, 'other-item', ?, ?)",
    )
    .bind(&sha)
    .bind(image_id)
    .bind(until)
    .bind(&key)
    .execute(&pool)
    .await
    .unwrap();
    let (status, held) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(held["code"], "LEASE_HELD");
    assert_eq!(held["retryable"], true);
    let images: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images WHERE source_sha256 = ?")
        .bind(&sha)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(images.0, 0);
    std::fs::create_dir_all(dir.path().join("media").join(&key).parent().unwrap()).unwrap();
    std::fs::write(dir.path().join("media").join(&key), &png).unwrap();
    sqlx::query("UPDATE original_reservations SET lease_until = 0 WHERE source_sha256 = ?")
        .bind(&sha)
        .execute(&pool)
        .await
        .unwrap();
    let (status, committed) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(committed["image_id"], image_id);
    let left: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM original_reservations WHERE source_sha256 = ?")
        .bind(&sha)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left.0, 0);
    let stored = std::fs::read(dir.path().join("media").join(&key)).unwrap();
    assert_eq!(stored, png);
}

#[tokio::test]
async fn one_failed_download_can_be_claimed_again_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Pick"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let first_id = upload_png(&h, album_id, "a.png", 8).await;
    let second_id = upload_png(&h, album_id, "b.png", 12).await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE media_locations SET state = 'failed'
         WHERE blob_id = (SELECT original_blob_id FROM images WHERE id = ?)",
    )
    .bind(&second_id)
    .execute(&pool)
    .await
    .unwrap();
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/downloads/selected"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "batch-1")
                .body(Body::from(
                    serde_json::json!({"image_ids": [first_id, second_id]}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch["items"][0]["ok"], true);
    assert_eq!(batch["items"][1]["ok"], false);
    assert_eq!(batch["items"][1]["code"], "MEDIA_MISSING");
    let kept = batch["items"][0]["claim_id"].as_str().unwrap();
    let (status, _, _) = h
        .call(
            Request::get(format!("/api/v1/downloads/claims/{kept}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query(
        "UPDATE media_locations SET state = 'verified'
         WHERE blob_id = (SELECT original_blob_id FROM images WHERE id = ?)",
    )
    .bind(&second_id)
    .execute(&pool)
    .await
    .unwrap();
    let (status, again) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/downloads/selected"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "only-second")
                .body(Body::from(serde_json::json!({"image_ids": [second_id]}).to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["items"][0]["image_id"], second_id);
    assert_eq!(again["items"][0]["ok"], true);
    let recovered = again["items"][0]["claim_id"].as_str().unwrap();
    assert_ne!(recovered, kept);
    let (status, _, _) = h
        .call(
            Request::get(format!("/api/v1/downloads/claims/{recovered}/body"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn expired_original_reservation_is_removed_when_nothing_indexes_it() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let now = gallery::db::now_ms();
    storage.put_bytes("media/orphan.bin", b"orphan", "application/octet-stream").await.unwrap();
    storage.put_bytes("media/live.bin", b"live", "application/octet-stream").await.unwrap();
    storage.put_bytes("media/kept.bin", b"kept", "application/octet-stream").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let kept = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"kept"),
        4,
        "application/octet-stream",
        "original",
        &storage.backend_id,
        "media/kept.bin",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
         VALUES('img-kept', ?, ?, 'kept.bin', 8, 8, 'application/octet-stream', 'active', 1, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"kept"))
    .bind(&kept)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    for (sha, image_id, until, key) in [
        ("sha-orphan", "img-orphan", now - 1, "media/orphan.bin"),
        ("sha-live", "img-live", now + 60_000, "media/live.bin"),
        ("sha-indexed", "img-index-only", now - 1, "media/kept.bin"),
    ] {
        sqlx::query(
            "INSERT INTO original_reservations(source_sha256, image_id, lease_token, lease_until, planned_object_key)
             VALUES(?, ?, 'item', ?, ?)",
        )
        .bind(sha)
        .bind(image_id)
        .bind(until)
        .bind(key)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let removed = gallery::upload::reclaim_expired_reservations(&pool, &storage)
        .await
        .unwrap();
    assert_eq!(removed, 2);
    assert!(!dir.path().join("media").join("media/orphan.bin").is_file());
    assert!(dir.path().join("media").join("media/live.bin").is_file());
    assert!(dir.path().join("media").join("media/kept.bin").is_file());
    let left: Vec<(String,)> = sqlx::query_as("SELECT source_sha256 FROM original_reservations ORDER BY source_sha256")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].0, "sha-live");
}

#[tokio::test]
async fn caption_and_album_edits_update_the_search_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Plain Shelf","description":"spare"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "plain.png", 8).await;
    let (status, rebuild) = h
        .json(Request::post("/api/v1/admin/search/rebuild").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job_id = rebuild["job_id"].as_str().unwrap();
    let mut job_state = String::new();
    for _ in 0..50 {
        let (_, job) = h
            .json(Request::get(format!("/api/v1/jobs/{job_id}")).body(Body::empty()).unwrap())
            .await;
        job_state = job["state"].as_str().unwrap().to_string();
        if job_state == "succeeded" || job_state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(job_state, "succeeded");
    let (status, saved) = h
        .json(
            Request::patch(format!("/api/v1/images/{image_id}/caption"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"text":"violetbloom shore","expected_revision":1}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["source"], "human");
    let (status, found) = h
        .json(Request::get("/api/v1/search?q=violetbloom").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(found["items"]["images"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == image_id));
    let (status, current) = h
        .json(Request::get(format!("/api/v1/albums/{album_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, renamed) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"name":"AmberHarbor","expected_revision": current["revision"].as_i64().unwrap()}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let _ = renamed;
    let (status, albums) = h
        .json(Request::get("/api/v1/search?q=amberharbor").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(albums["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == album_id));
}

#[tokio::test]
async fn moving_an_image_keeps_it_in_the_search_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, source) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"From Shelf","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let source_id = source["id"].as_str().unwrap();
    let (status, target) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"To Shelf","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let target_id = target["id"].as_str().unwrap();
    let image_id = upload_png(&h, source_id, "harbor.png", 8).await;
    let (status, rebuild) = h
        .json(Request::post("/api/v1/admin/search/rebuild").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job_id = rebuild["job_id"].as_str().unwrap();
    let mut job_state = String::new();
    for _ in 0..50 {
        let (_, job) = h
            .json(Request::get(format!("/api/v1/jobs/{job_id}")).body(Body::empty()).unwrap())
            .await;
        job_state = job["state"].as_str().unwrap().to_string();
        if job_state == "succeeded" || job_state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(job_state, "succeeded");
    let (status, source_now) = h
        .json(Request::get(format!("/api/v1/albums/{source_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, target_now) = h
        .json(Request::get(format!("/api/v1/albums/{target_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, moved) = h
        .json(
            Request::post(format!("/api/v1/albums/{source_id}/items/move"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "target_album_id": target_id,
                        "expected_revision": source_now["revision"],
                        "target_expected_revision": target_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let _ = moved;
    let (status, found) = h
        .json(Request::get("/api/v1/search?q=harbor").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let images = found["items"]["images"].as_array().unwrap();
    assert!(images.iter().any(|item| item["id"] == image_id && item["album_id"] == target_id));
}

#[tokio::test]
async fn unknown_delivery_is_not_resent_until_asked() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    gallery::auth::create_owner(&pool, "admin", "owner-pass-12").await.unwrap();
    let owner: (String,) = sqlx::query_as("SELECT id FROM actors WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0.clone(),
        username: "admin".into(),
        role: "owner".into(),
        must_change_password: false,
        revision: 1,
    };
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, bridge_token, created_at, updated_at)
         VALUES('sess-unknown', ?, '{}', 'idle', 'bridge', ?, ?)",
    )
    .bind(&actor.id)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    for (id, seq, client, state) in [
        ("msg-unknown", 1_i64, "client-unknown", "delivery_unknown"),
        ("msg-paused", 2, "client-paused", "paused"),
    ] {
        sqlx::query(
            "INSERT INTO agent_messages(id, session_id, seq, client_message_id, mode, delivery_state, body, created_at)
             VALUES(?, 'sess-unknown', ?, ?, 'steer_if_active', ?, 'same words', ?)",
        )
        .bind(id)
        .bind(seq)
        .bind(client)
        .bind(state)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    let resumed = gallery::ai::resume_pending(&pool, &actor, "sess-unknown")
        .await
        .unwrap();
    assert_eq!(resumed["resumed"], 1);
    let unknown: (String,) = sqlx::query_as(
        "SELECT delivery_state FROM agent_messages WHERE id = 'msg-unknown'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unknown.0, "delivery_unknown");
    let posted = gallery::ai::post_message(
        &pool,
        &actor,
        "sess-unknown",
        gallery::ai::IncomingMessage {
            client_message_id: "client-again".into(),
            text: "same words".into(),
            mode: None,
            expected_turn_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(posted.0, 202);
    assert_ne!(posted.1["id"], "msg-unknown");
    let resent = gallery::ai::resend_unknown(&pool, &actor, "sess-unknown", "msg-unknown", "client-resend")
        .await
        .unwrap();
    assert_eq!(resent["delivery_state"], "queued");
    assert_eq!(resent["resent_from"], "msg-unknown");
    assert_ne!(resent["id"], posted.1["id"]);
    let again = gallery::ai::resend_unknown(&pool, &actor, "sess-unknown", "msg-unknown", "client-resend")
        .await
        .unwrap();
    assert_eq!(again["id"], resent["id"]);
    let still: (String,) = sqlx::query_as(
        "SELECT delivery_state FROM agent_messages WHERE id = 'msg-unknown'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(still.0, "delivery_unknown");
    let refused = gallery::ai::resend_unknown(&pool, &actor, "sess-unknown", "msg-paused", "client-no")
        .await;
    assert!(refused.unwrap_err().to_string().contains("MESSAGE_STATE"));
}

#[tokio::test]
async fn new_album_and_upload_show_up_in_the_search_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, rebuild) = h
        .json(Request::post("/api/v1/admin/search/rebuild").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job_id = rebuild["job_id"].as_str().unwrap();
    let mut job_state = String::new();
    for _ in 0..50 {
        let (_, job) = h
            .json(Request::get(format!("/api/v1/jobs/{job_id}")).body(Body::empty()).unwrap())
            .await;
        job_state = job["state"].as_str().unwrap().to_string();
        if job_state == "succeeded" || job_state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(job_state, "succeeded");
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"CedarRoom","description":"quiet"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let (status, albums) = h
        .json(Request::get("/api/v1/search?q=cedarroom").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(albums["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == album_id));
    let image_id = upload_png(&h, album_id, "marigold.png", 8).await;
    let (status, images) = h
        .json(Request::get("/api/v1/search?q=marigold").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(images["items"]["images"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == image_id && item["album_id"] == album_id));
}

#[tokio::test]
async fn trashed_cover_falls_back_and_restore_rebuilds_variants() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Covers","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let first = upload_png(&h, album_id, "first.png", 8).await;
    let second = upload_png(&h, album_id, "second.png", 12).await;
    let (status, current) = h
        .json(Request::get(format!("/api/v1/albums/{album_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, patched) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "cover_image_id": second,
                        "expected_revision": current["revision"].as_i64().unwrap()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["cover_image_id"], second);
    let (status, trashed) = h
        .json(
            Request::post(format!("/api/v1/images/{second}/trash"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_lifecycle_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trashed["lifecycle_revision"], 2);
    let (status, fallen) = h
        .json(Request::get(format!("/api/v1/albums/{album_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fallen["cover_image_id"], first);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query("DELETE FROM variant_heads WHERE image_id = ?")
        .bind(&second)
        .execute(&pool)
        .await
        .unwrap();
    let (status, restored) = h
        .json(
            Request::post(format!("/api/v1/images/{second}/restore"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_lifecycle_revision":2}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restored["state"], "active");
    let (jobs,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE dedupe_key LIKE ?",
    )
    .bind(format!("restore_variant:{second}:%"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(jobs >= 1);
}

#[tokio::test]
async fn trash_older_than_thirty_days_is_queued_for_purge() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    for (id, deleted_at, seq) in [
        ("img-old", now - gallery::album::TRASH_RETAIN_MS - 1_000, 1_i64),
        ("img-new", now - 1_000, 2),
    ] {
        let label = id.as_bytes();
        let blob = gallery::storage::record_blob(
            &mut tx,
            &gallery::ids::sha256_hex(label),
            label.len() as i64,
            "application/octet-stream",
            "original",
            &storage.backend_id,
            &format!("media/{id}.bin"),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, deleted_at, created_seq, created_at)
             VALUES(?, ?, ?, ?, 8, 8, 'application/octet-stream', 'trashed', ?, ?, ?)",
        )
        .bind(id)
        .bind(gallery::ids::sha256_hex(label))
        .bind(&blob)
        .bind(format!("{id}.bin"))
        .bind(deleted_at)
        .bind(seq)
        .bind(deleted_at)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let queued = gallery::album::enqueue_expired_trash(&pool).await.unwrap();
    assert_eq!(queued, vec!["img-old".to_string()]);
    let old: (String,) = sqlx::query_as("SELECT state FROM images WHERE id = 'img-old'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old.0, "purging");
    let fresh: (String,) = sqlx::query_as("SELECT state FROM images WHERE id = 'img-new'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fresh.0, "trashed");
    let (jobs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE dedupe_key = 'purge:img-old'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 1);
    let again = gallery::album::enqueue_expired_trash(&pool).await.unwrap();
    assert!(again.is_empty());
    let (jobs_after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE dedupe_key = 'purge:img-old'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs_after, 1);
}

#[tokio::test]
async fn display_name_is_private_to_one_album() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, first) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"One","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let first_id = first["id"].as_str().unwrap();
    let (status, second) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Two","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let second_id = second["id"].as_str().unwrap();
    let image_id = upload_png(&h, first_id, "harbor.png", 8).await;
    let (status, second_now) = h
        .json(Request::get(format!("/api/v1/albums/{second_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, added) = h
        .json(
            Request::post(format!("/api/v1/albums/{second_id}/items/add"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_ids": [image_id],
                        "expected_revision": second_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let _ = added;
    let (status, rebuild) = h
        .json(Request::post("/api/v1/admin/search/rebuild").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job_id = rebuild["job_id"].as_str().unwrap();
    let mut job_state = String::new();
    for _ in 0..50 {
        let (_, job) = h
            .json(Request::get(format!("/api/v1/jobs/{job_id}")).body(Body::empty()).unwrap())
            .await;
        job_state = job["state"].as_str().unwrap().to_string();
        if job_state == "succeeded" || job_state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(job_state, "succeeded");
    let (status, first_now) = h
        .json(Request::get(format!("/api/v1/albums/{first_id}")).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, renamed) = h
        .json(
            Request::post(format!("/api/v1/albums/{first_id}/items/rename"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image_id": image_id,
                        "display_name": "NorthPier",
                        "expected_revision": first_now["revision"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renamed["display_name"], "NorthPier");
    let (status, in_first) = h
        .json(
            Request::get(format!("/api/v1/albums/{first_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(in_first["items"][0]["display_name"], "NorthPier");
    let (status, in_second) = h
        .json(
            Request::get(format!("/api/v1/albums/{second_id}/images"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(in_second["items"][0]["display_name"], "harbor.png");
    let (status, by_new) = h
        .json(Request::get("/api/v1/search?q=northpier").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(by_new["items"]["images"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == image_id && item["album_id"] == first_id));
    let (status, by_old) = h
        .json(Request::get("/api/v1/search?q=harbor").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let old_hits = by_old["items"]["images"].as_array().unwrap();
    assert!(old_hits.iter().any(|item| item["id"] == image_id && item["album_id"] == second_id));
    assert!(!old_hits.iter().any(|item| item["id"] == image_id && item["album_id"] == first_id));
}

#[tokio::test]
async fn a_committed_item_rejects_a_different_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Once","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let first = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size":first.len()}]}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(first.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap();
    let other = png_bytes(12, 8, false);
    let (status, rejected) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(other))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rejected["code"], "ITEM_SHA");
    let (status, replay) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(first.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["image_id"], image_id);
    let (status, _, bytes) = h
        .call(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}/original-download"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, first);
}

#[tokio::test]
async fn missing_desktop_variant_falls_back_to_the_next_smaller_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Sizes","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO job_pauses(kind, reason, paused_at) VALUES('generate_variant', 'test', ?)",
    )
    .bind(gallery::db::now_ms())
    .execute(&pool)
    .await
    .unwrap();
    let image_id = upload_png(&h, album_id, "sizes.png", 8).await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (blob,): (String,) = sqlx::query_as("SELECT original_blob_id FROM images WHERE id = ?")
        .bind(&image_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM variant_heads WHERE image_id = ?")
        .bind(&image_id)
        .execute(&pool)
        .await
        .unwrap();
    for (id, spec, long) in [("var-thumb", "thumb", 256_i64), ("var-tablet", "tablet", 1440)] {
        sqlx::query(
            "INSERT INTO image_variants(id, image_id, profile_revision, spec_id, blob_id, state, width, height)
             VALUES(?, ?, 1, ?, ?, 'published', ?, ?)
             ON CONFLICT(image_id, profile_revision, spec_id) DO UPDATE SET
                blob_id = excluded.blob_id, state = 'published', width = excluded.width, height = excluded.height",
        )
        .bind(id)
        .bind(&image_id)
        .bind(spec)
        .bind(&blob)
        .bind(long)
        .bind(long / 2)
        .execute(&pool)
        .await
        .unwrap();
        let (variant_id,): (String,) = sqlx::query_as(
            "SELECT id FROM image_variants WHERE image_id = ? AND profile_revision = 1 AND spec_id = ?",
        )
        .bind(&image_id)
        .bind(spec)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO variant_heads(image_id, spec_id, variant_id, published_at)
             VALUES(?, ?, ?, ?)
             ON CONFLICT(image_id, spec_id) DO UPDATE SET variant_id = excluded.variant_id",
        )
        .bind(&image_id)
        .bind(spec)
        .bind(&variant_id)
        .bind(gallery::db::now_ms())
        .execute(&pool)
        .await
        .unwrap();
    }
    let (status, detail) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["selection"]["desktop"]["spec_id"], "tablet");
    assert_eq!(detail["selection"]["desktop"]["fallback"], true);
    assert_eq!(detail["selection"]["tablet"]["fallback"], false);
    assert_eq!(detail["selection"]["mobile"]["spec_id"], "tablet");
    assert_eq!(detail["selection"]["mobile"]["fallback"], true);
}

#[tokio::test]
async fn a_late_old_variant_does_not_replace_the_newer_head() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let blob = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"variant"),
        7,
        "image/png",
        "variant",
        &storage.backend_id,
        "media/variant.png",
    )
    .await
    .unwrap();
    let original = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"original"),
        8,
        "image/png",
        "original",
        &storage.backend_id,
        "media/original.png",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
         VALUES('img-late', ?, ?, 'late.png', 8, 8, 'image/png', 'active', 1, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"original"))
    .bind(&original)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    for (id, revision) in [("var-new", 2_i64), ("var-old", 1)] {
        sqlx::query(
            "INSERT INTO image_variants(id, image_id, profile_revision, spec_id, blob_id, state, width, height)
             VALUES(?, 'img-late', ?, 'thumb', ?, 'published', 256, 256)",
        )
        .bind(id)
        .bind(revision)
        .bind(&blob)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO variant_heads(image_id, spec_id, variant_id, published_at)
         VALUES('img-late', 'thumb', 'var-new', ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    let late = gallery::versions::claim_variant_head(&mut tx, "img-late", "thumb", "var-old", 1)
        .await
        .unwrap();
    assert!(!late);
    let head: (String,) = sqlx::query_as(
        "SELECT variant_id FROM variant_heads WHERE image_id = 'img-late' AND spec_id = 'thumb'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(head.0, "var-new");
    let newer = gallery::versions::claim_variant_head(&mut tx, "img-late", "thumb", "var-old", 3)
        .await
        .unwrap();
    assert!(newer);
    let head: (String,) = sqlx::query_as(
        "SELECT variant_id FROM variant_heads WHERE image_id = 'img-late' AND spec_id = 'thumb'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(head.0, "var-old");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn expired_variant_bytes_go_away_when_nothing_points_at_them() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite")).await.unwrap();
    let storage = gallery::storage::bootstrap_local(&pool, dir.path().join("media")).await.unwrap();
    let now = gallery::db::now_ms();
    storage.put_bytes("media/old.png", b"old", "image/png").await.unwrap();
    storage.put_bytes("media/held.png", b"held", "image/png").await.unwrap();
    storage.put_bytes("media/fresh.png", b"fresh", "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    let original = gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"original"),
        8,
        "image/png",
        "original",
        &storage.backend_id,
        "media/original.png",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
         VALUES('img-retain', ?, ?, 'retain.png', 8, 8, 'image/png', 'active', 1, ?)",
    )
    .bind(gallery::ids::sha256_hex(b"original"))
    .bind(&original)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    for (id, key, bytes, retain, revision, spec) in [
        ("var-old", "media/old.png", b"old".as_slice(), Some(now - 1), 1_i64, "thumb"),
        ("var-head", "media/held.png", b"held".as_slice(), Some(now - 1), 2, "thumb"),
        ("var-fresh", "media/fresh.png", b"fresh".as_slice(), Some(now + 86_400_000), 2, "mobile"),
    ] {
        let blob = gallery::storage::record_blob(
            &mut tx,
            &gallery::ids::sha256_hex(bytes),
            bytes.len() as i64,
            "image/png",
            "variant",
            &storage.backend_id,
            key,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO image_variants(id, image_id, profile_revision, spec_id, blob_id, state, width, height, retain_until)
             VALUES(?, 'img-retain', ?, ?, ?, 'published', 256, 256, ?)",
        )
        .bind(id)
        .bind(revision)
        .bind(spec)
        .bind(&blob)
        .bind(retain)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO variant_heads(image_id, spec_id, variant_id, published_at)
         VALUES('img-retain', 'thumb', 'var-head', ?)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let removed = gallery::versions::reclaim_expired_variants(&pool, &storage)
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert!(!dir.path().join("media").join("media/old.png").is_file());
    assert!(dir.path().join("media").join("media/held.png").is_file());
    assert!(dir.path().join("media").join("media/fresh.png").is_file());
    let old: (String,) = sqlx::query_as("SELECT state FROM image_variants WHERE id = 'var-old'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old.0, "superseded");
    let head: (String,) = sqlx::query_as("SELECT state FROM image_variants WHERE id = 'var-head'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head.0, "published");
}

#[tokio::test]
async fn variant_stats_count_published_backlog_and_missing_previews() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let storage = gallery::storage::open_routed(&pool).await.unwrap();
    let now = gallery::db::now_ms();
    let mut tx = gallery::db::begin_immediate(&pool).await.unwrap();
    for (id, label, seq) in [("img-ready", "ready", 1_i64), ("img-missing", "missing", 2)] {
        let blob = gallery::storage::record_blob(
            &mut tx,
            &gallery::ids::sha256_hex(label.as_bytes()),
            label.len() as i64,
            "image/png",
            "original",
            &storage.backend_id,
            &format!("media/{id}.png"),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, state, created_seq, created_at)
             VALUES(?, ?, ?, ?, 8, 8, 'image/png', 'active', ?, ?)",
        )
        .bind(id)
        .bind(gallery::ids::sha256_hex(label.as_bytes()))
        .bind(&blob)
        .bind(format!("{label}.png"))
        .bind(seq)
        .bind(now)
        .execute(&mut *tx)
        .await
        .unwrap();
        if id == "img-ready" {
            sqlx::query(
                "INSERT INTO image_variants(id, image_id, profile_revision, spec_id, blob_id, state, width, height)
                 VALUES('var-stats-thumb', ?, 1, 'thumb', ?, 'published', 256, 256)",
            )
            .bind(id)
            .bind(&blob)
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO variant_heads(image_id, spec_id, variant_id, published_at)
                 VALUES(?, 'thumb', 'var-stats-thumb', ?)",
            )
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
    }
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, priority, available_at, created_at, updated_at)
         VALUES('job-stats', 'generate_variant', 'stats-backlog', '{}', '{}', 'queued', 10, ?, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (status, stats) = h
        .json(Request::get("/api/v1/admin/variants/stats").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["active_images"], 2);
    assert_eq!(stats["spec_count"], 4);
    assert_eq!(stats["published"], 1);
    assert_eq!(stats["published_ratio"], 0.125);
    assert_eq!(stats["backlog"], 1);
    assert_eq!(stats["preview_missing"], 1);
    assert_eq!(stats["preview_missing_rate"], 0.5);
}

#[tokio::test]
async fn staging_quota_is_separate_from_stored_originals() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Quota","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('limit_staging_bytes', '20')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    let batch = |size: u64, name: &str| {
        let name = name.to_string();
        serde_json::json!({"items":[{"client_item_id": name, "name": format!("{name}.png"), "size": size}]}).to_string()
    };
    let (status, _) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(batch(15, "hold")))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, blocked) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(batch(15, "more")))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(blocked["code"], "STAGING_QUOTA");
    sqlx::query("DELETE FROM schema_app WHERE key = 'limit_staging_bytes'")
        .execute(&pool)
        .await
        .unwrap();
    let first = png_bytes(8, 8, false);
    let (status, created) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(batch(first.len() as u64, "stored")))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let batch_id = created["id"].as_str().unwrap();
    let item_id = created["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(first.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let image_id = commit["image_id"].as_str().unwrap();
    let (size,): (i64,) = sqlx::query_as(
        "SELECT size FROM media_blobs WHERE id = (SELECT original_blob_id FROM images WHERE id = ?)",
    )
    .bind(image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES('limit_library_bytes', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(size.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let other = png_bytes(12, 8, false);
    let (status, next) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(batch(other.len() as u64, "extra")))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let next_batch = next["id"].as_str().unwrap();
    let next_item = next["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, full) = h
        .json(
            Request::put(format!("/api/v1/uploads/{next_batch}/items/{next_item}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(other))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(full["code"], "LIBRARY_QUOTA");
    let (status, again) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(batch(first.len() as u64, "same")))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let same_batch = again["id"].as_str().unwrap();
    let same_item = again["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, reused) = h
        .json(
            Request::put(format!("/api/v1/uploads/{same_batch}/items/{same_item}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(first))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(reused["image_id"], image_id);
}

#[tokio::test]
async fn variant_cache_is_private_and_originals_are_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let mut specs = gallery::versions::default_specs();
    specs[0].format = "jpeg".into();
    let (status, _) = h
        .json(
            Request::put("/api/v1/variant-profiles/current")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "\"1\"")
                .body(Body::from(serde_json::to_vec(&specs).unwrap()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Cache"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "cache.png", 16).await;
    let thumb = wait_thumb(&h, album_id).await;
    let path = format!("/api/v1/albums/{album_id}/images/{image_id}/variants/{thumb}/body");
    let (status, headers, bytes) = h
        .call(Request::get(&path).body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert!(bytes.starts_with(&[0xff, 0xd8]));
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-cache"
    );
    let etag = headers.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"') && etag.len() > 2);
    assert_eq!(headers.get(header::ACCEPT_RANGES).unwrap(), "bytes");
    let (status, headers, body) = h
        .call(
            Request::get(&path)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    assert_eq!(headers.get(header::ETAG).unwrap().to_str().unwrap(), etag);
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-cache"
    );
    let (status, headers, ranged) = h
        .call(
            Request::get(&path)
                .header(header::RANGE, "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.len(), 4);
    assert_eq!(&bytes[..4], ranged.as_slice());
    assert!(headers
        .get(header::CONTENT_RANGE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("bytes 0-3/"));
    let (status, headers, _) = h
        .call(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/original-view"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    h.cookie.clear();
    let (status, _, _) = h
        .call(
            Request::get(&path)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn identical_detail_variants_share_one_object_and_srcset_width() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Srcset"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "tiny.png", 8).await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let mut blobs: Vec<(String, String)> = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        blobs = sqlx::query_as(
            "SELECT v.spec_id, v.blob_id
             FROM variant_heads h
             JOIN image_variants v ON v.id = h.variant_id
             WHERE h.image_id = ? AND v.spec_id IN ('mobile', 'tablet', 'desktop')
             ORDER BY v.spec_id",
        )
        .bind(&image_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        if blobs.len() == 3 && blobs.iter().all(|row| row.1 == blobs[0].1) {
            break;
        }
    }
    assert_eq!(blobs.len(), 3, "{blobs:?}");
    assert!(blobs.iter().all(|row| row.1 == blobs[0].1), "{blobs:?}");
    let (keys,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT l.object_key)
         FROM media_locations l
         WHERE l.blob_id = ? AND l.state = 'verified'",
    )
    .bind(&blobs[0].1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(keys, 1);
    let (status, image) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}/images/{image_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(image["placeholder"], false);
    let srcset = image["srcset"].as_array().unwrap();
    assert_eq!(srcset.len(), 1);
    assert_eq!(srcset[0]["spec_id"], "mobile");
    assert_eq!(srcset[0]["width"], 8);
    assert!(srcset[0].get("url").is_none());
    assert_ne!(image["selection"]["thumb"]["variant_id"], srcset[0]["variant_id"]);
}

#[tokio::test]
async fn configured_password_revision_does_not_reset_a_changed_password() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_username = "admin".into();
    cfg.admin_password = Some("owner-pass-12".into());
    cfg.admin_password_file = None;
    cfg.admin_password_revision = None;
    cfg.ai_enabled = false;
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let pool = &state.pool;
    let keys = dir.path().join("keys");
    let mode = std::fs::metadata(keys.join("deploy.key")).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.mode() & 0o777, 0o600);
    let hash = || async {
        let (hash,): (String,) =
            sqlx::query_as("SELECT password_hash FROM actors WHERE role = 'owner'")
                .fetch_one(pool)
                .await
                .unwrap();
        hash
    };
    assert!(gallery::auth::verify_password(&hash().await, "owner-pass-12"));
    let changed = gallery::auth::hash_password("changed-pass-1").unwrap();
    sqlx::query("UPDATE actors SET password_hash = ? WHERE role = 'owner'")
        .bind(&changed)
        .execute(pool)
        .await
        .unwrap();
    let again = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("owner-pass-12"),
        None,
        "env",
        false,
    )
    .await
    .unwrap();
    assert_eq!(again, gallery::auth::PasswordSync::Unchanged);
    assert!(gallery::auth::verify_password(&hash().await, "changed-pass-1"));
    let applied = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("owner-pass-12"),
        Some("2026-11-23-a"),
        "env",
        false,
    )
    .await
    .unwrap();
    assert_eq!(applied, gallery::auth::PasswordSync::Applied);
    assert!(gallery::auth::verify_password(&hash().await, "owner-pass-12"));
    let replay = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("owner-pass-12"),
        Some("2026-11-23-a"),
        "env",
        false,
    )
    .await
    .unwrap();
    assert_eq!(replay, gallery::auth::PasswordSync::Unchanged);
    sqlx::query("UPDATE actors SET password_hash = ? WHERE role = 'owner'")
        .bind(&changed)
        .execute(pool)
        .await
        .unwrap();
    let kept = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("owner-pass-12"),
        Some("2026-11-23-a"),
        "env",
        false,
    )
    .await
    .unwrap();
    assert_eq!(kept, gallery::auth::PasswordSync::Unchanged);
    assert!(gallery::auth::verify_password(&hash().await, "changed-pass-1"));
    let conflict = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("other-secret-1"),
        Some("2026-11-23-a"),
        "env",
        false,
    )
    .await;
    assert!(conflict.unwrap_err().to_string().contains("revision rejected"));
    assert!(gallery::auth::verify_password(&hash().await, "changed-pass-1"));
    let weak = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("short"),
        Some("2026-11-23-b"),
        "env",
        false,
    )
    .await;
    assert!(weak.unwrap_err().to_string().contains("password rejected"));
    assert!(gallery::auth::verify_password(&hash().await, "changed-pass-1"));
    std::fs::write(keys.join("deploy.key"), [9u8; 32]).unwrap();
    let rebound = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("owner-pass-12"),
        Some("2026-11-23-c"),
        "env",
        false,
    )
    .await
    .unwrap();
    assert_eq!(rebound, gallery::auth::PasswordSync::NeedsRebind);
    assert!(gallery::auth::verify_password(&hash().await, "changed-pass-1"));
    let forced = gallery::auth::sync_configured_password(
        pool,
        &keys,
        "admin",
        Some("owner-pass-12"),
        Some("2026-11-23-c"),
        "env",
        true,
    )
    .await
    .unwrap();
    assert_eq!(forced, gallery::auth::PasswordSync::Applied);
    assert!(gallery::auth::verify_password(&hash().await, "owner-pass-12"));
    let cleared = gallery::auth::sync_configured_password(pool, &keys, "admin", None, None, "env", false)
        .await
        .unwrap();
    assert_eq!(cleared, gallery::auth::PasswordSync::Unchanged);
    assert!(gallery::auth::verify_password(&hash().await, "owner-pass-12"));
}

#[tokio::test]
async fn login_and_setup_failures_are_rate_limited_by_source() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path().to_path_buf()).await;
    async fn login(h: &Harness, password: &str, source: &str) -> (StatusCode, header::HeaderMap, Vec<u8>) {
        h.call(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-forwarded-for", source)
                .body(Body::from(format!(
                    r#"{{"username":"admin","password":"{password}"}}"#
                )))
                .unwrap(),
        )
        .await
    }
    for _ in 0..4 {
        let (status, _, _) = login(&h, "wrong-pass-1", "203.0.113.8").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, _) = login(&h, "owner-pass-12", "203.0.113.8").await;
    assert_eq!(status, StatusCode::CREATED);
    for _ in 0..4 {
        let (status, _, _) = login(&h, "wrong-pass-1", "203.0.113.8").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, headers, body) = login(&h, "wrong-pass-1", "203.0.113.8").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let problem: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["code"], "RATE_LIMIT");
    assert_eq!(problem["retryable"], true);
    let retry_after = headers.get(header::RETRY_AFTER).unwrap().to_str().unwrap();
    assert_eq!(problem["retry_after_ms"], retry_after.parse::<u64>().unwrap() * 1000);
    let (status, _, _) = login(&h, "wrong-pass-1", "203.0.113.9").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let setup_dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = setup_dir.path().to_path_buf();
    cfg.admin_password = None;
    cfg.admin_password_file = None;
    cfg.admin_password_revision = None;
    cfg.ai_enabled = false;
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let setup_app = gallery::scheduler::router(state)
        .layer(axum::middleware::from_fn(gallery::http_util::request_id_mw));
    let setup = |source: &str| {
        let source = source.to_string();
        let app = setup_app.clone();
        async move {
            let res = app
                .oneshot(
                    Request::post("/api/v1/setup/owner")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("x-forwarded-for", source)
                        .body(Body::from(
                            r#"{"bootstrap_token":"nope","password":"owner-pass-12"}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status();
            let headers = res.headers().clone();
            let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
            (status, headers, body)
        }
    };
    for _ in 0..4 {
        let (status, _, _) = setup("198.51.100.4").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, headers, body) = setup("198.51.100.4").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let problem: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["code"], "RATE_LIMIT");
    assert!(headers.get(header::RETRY_AFTER).is_some());
    let (status, _, _) = setup("198.51.100.5").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_aliases_redirect_and_reject_unsafe_next() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    for alias in [
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
        "/WP-ADMIN",
    ] {
        let (status, headers, _) = h
            .call(
                Request::get(format!("{alias}?next=https://evil.example"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT, "{alias}");
        assert_eq!(headers.get(header::LOCATION).unwrap(), "/admin");
    }
    let (status, headers, _) = h
        .call(Request::head("/wp-admin").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/admin");
    let (status, headers, _) = h
        .call(Request::post("/wp-admin").body(Body::empty()).unwrap())
        .await;
    assert_ne!(status, StatusCode::PERMANENT_REDIRECT);
    assert!(headers.get(header::LOCATION).is_none());
    for blocked in ["/api/v1/admin", "/albums/foo/admin", "/%77p-admin"] {
        let (status, headers, _) = h
            .call(Request::get(blocked).body(Body::empty()).unwrap())
            .await;
        assert_ne!(status, StatusCode::PERMANENT_REDIRECT, "{blocked}");
        assert_ne!(status, StatusCode::SEE_OTHER, "{blocked}");
        assert!(headers.get(header::LOCATION).is_none(), "{blocked}");
    }
    let (status, headers, _) = h
        .call(
            Request::get("/admin?next=https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/settings/general");
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    let (status, headers, _) = h
        .call(
            Request::get("/admin?next=%2Falbums%2Fkept")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/albums/kept");
    h.cookie.clear();
    let (status, headers, _) = h
        .call(
            Request::get("/admin?next=https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/login");
    let (status, headers, _) = h
        .call(
            Request::get("/admin?next=%2Falbums%2Fkept")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap(),
        "/login?next=%2Falbums%2Fkept"
    );
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO actors(id, username, password_hash, role, status, must_change_password, revision, created_at)
         VALUES('actor-member', 'member', 'x', 'collaborator', 'active', 0, 1, ?)",
    )
    .bind(gallery::db::now_ms())
    .execute(&pool)
    .await
    .unwrap();
    let member = gallery::auth::Actor {
        id: "actor-member".into(),
        username: "member".into(),
        role: "collaborator".into(),
        must_change_password: false,
        revision: 1,
    };
    let (token, _) = gallery::auth::issue_session(&pool, &member, 60_000)
        .await
        .unwrap();
    h.cookie = token;
    let (status, headers, _) = h
        .call(
            Request::get("/admin?next=%2Fsettings%2Fgeneral")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/admin/unavailable");
    let setup_dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = setup_dir.path().to_path_buf();
    cfg.admin_password = None;
    cfg.admin_password_file = None;
    cfg.admin_password_revision = None;
    cfg.ai_enabled = false;
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let app = gallery::scheduler::router(state)
        .layer(axum::middleware::from_fn(gallery::http_util::request_id_mw));
    let res = app
        .oneshot(Request::get("/admin").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/admin/setup");
    assert_eq!(res.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
}

#[tokio::test]
async fn backup_records_written_read_back_and_verified_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Backup stats"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    upload_png(&h, album_id, "kept.png", 24).await;
    let (status, started) = h
        .json(
            Request::post("/api/v1/admin/backups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let id = started["id"].as_str().unwrap();
    let mut detail = Value::Null;
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status, body) = h
            .json(
                Request::get(format!("/api/v1/admin/backups/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        if body["state"] == "succeeded" {
            detail = body;
            break;
        }
    }
    assert_eq!(detail["state"], "succeeded");
    let written = detail["bytes_written"].as_i64().unwrap();
    let read_back = detail["bytes_read_back"].as_i64().unwrap();
    let verified = detail["bytes_verified"].as_i64().unwrap();
    assert!(written > 0);
    assert_eq!(read_back, written);
    assert!(verified > 0);
    let (status, listed) = h
        .json(Request::get("/api/v1/admin/backups").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let item = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == id)
        .unwrap();
    assert_eq!(item["bytes_written"], written);
    assert_eq!(item["bytes_read_back"], read_back);
    assert_eq!(item["bytes_verified"], verified);
}

#[tokio::test]
async fn retry_opens_a_new_attempt_round_and_keeps_history() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite"))
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, actor_id, state, available_at, attempt_no, max_attempts, failure_code, run_generation, created_at, updated_at)
         VALUES('job-repair', 'backup', 'repair-1', '{}', '{}', 'owner-1', 'failed', ?, 3, 3, 'JOB_FAILED', 1, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    for attempt in 1..=3 {
        sqlx::query(
            "INSERT INTO job_attempts(job_id, attempt_no, worker_id, started_at, finished_at, result)
             VALUES('job-repair', ?, 'worker-a', ?, ?, 'failed')",
        )
        .bind(attempt)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    let view = gallery::jobs::retry(&pool, "job-repair").await.unwrap().unwrap();
    assert_eq!(view.state, "queued");
    let row: (i64, i64, i64, Option<String>) = sqlx::query_as(
        "SELECT attempt_no, max_attempts, run_generation, failure_code FROM jobs WHERE id = 'job-repair'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, 3);
    assert_eq!(row.1, 6);
    assert_eq!(row.2, 2);
    assert!(row.3.is_none());
    let claimed = gallery::jobs::claim_next(&pool, "worker-b").await.unwrap();
    assert_eq!(claimed.unwrap().0, "job-repair");
    let attempts: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT attempt_no, result FROM job_attempts WHERE job_id = 'job-repair' ORDER BY attempt_no",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(attempts.len(), 4);
    assert_eq!(attempts[0], (1, Some("failed".into())));
    assert_eq!(attempts[2], (3, Some("failed".into())));
    assert_eq!(attempts[3].0, 4);
    assert!(attempts[3].1.is_none());
    let (actor,): (Option<String>,) =
        sqlx::query_as("SELECT actor_id FROM jobs WHERE id = 'job-repair'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(actor.as_deref(), Some("owner-1"));
}

#[tokio::test]
async fn lost_lease_removes_only_its_own_staging() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    cfg.admin_password_file = None;
    cfg.admin_password_revision = None;
    cfg.ai_enabled = false;
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let media = state.media().await.unwrap();
    media
        .put_bytes("media/published.bin", b"published-bytes", "application/octet-stream")
        .await
        .unwrap();
    media
        .put_bytes("media/staging.bin", b"staging-bytes", "application/octet-stream")
        .await
        .unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    gallery::storage::record_blob(
        &mut tx,
        &gallery::ids::sha256_hex(b"published-bytes"),
        b"published-bytes".len() as i64,
        "application/octet-stream",
        "original",
        &state.storage.backend_id,
        "media/published.bin",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let attempt = dir.path().join("work").join("codec").join("job-lost");
    let other = dir.path().join("work").join("codec").join("job-other");
    std::fs::create_dir_all(&attempt).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(attempt.join("original.bin"), b"temp").unwrap();
    std::fs::write(other.join("original.bin"), b"keep").unwrap();
    gallery::workers::discard_own_staging(&state, &attempt, Some("media/staging.bin"))
        .await
        .unwrap();
    assert!(!attempt.exists());
    assert_eq!(std::fs::read(other.join("original.bin")).unwrap(), b"keep");
    assert_eq!(
        media.get_bytes("media/published.bin").await.unwrap(),
        b"published-bytes"
    );
    assert!(media.get_bytes("media/staging.bin").await.is_err());
    let again = dir.path().join("work").join("codec").join("job-lost-2");
    std::fs::create_dir_all(&again).unwrap();
    gallery::workers::discard_own_staging(&state, &again, Some("media/published.bin"))
        .await
        .unwrap();
    assert!(!again.exists());
    assert_eq!(
        media.get_bytes("media/published.bin").await.unwrap(),
        b"published-bytes"
    );
}

#[tokio::test]
async fn deleting_an_album_records_the_change_and_drops_it_from_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Coastal Notes","description":"harbor"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let revision = album["revision"].as_i64().unwrap();
    let content_before = album["content_revision"].as_i64().unwrap();
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    gallery::search::rebuild(&pool).await.unwrap();
    let (status, found) = h
        .json(
            Request::get("/api/v1/search?q=coastal")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(found["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == album_id));
    let (status, _, _) = h
        .call(
            Request::delete(format!("/api/v1/albums/{album_id}"))
                .header(header::IF_MATCH, format!("\"{revision}\""))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, hidden) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(hidden["content_revision"].as_i64().unwrap() > content_before);
    let (audits,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'album.delete' AND target = ?",
    )
    .bind(album_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    let (events,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE type = 'album.deleted' AND resource_id = ?",
    )
    .bind(album_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    let (status, missing) = h
        .json(
            Request::get("/api/v1/search?q=coastal")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(missing["items"]["albums"].as_array().unwrap().is_empty());
    let deleted_revision = hidden["revision"].as_i64().unwrap();
    let (status, restored) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/restore"))
                .header(header::IF_MATCH, format!("\"{deleted_revision}\""))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(restored["content_revision"].as_i64().unwrap() > hidden["content_revision"].as_i64().unwrap());
    let (restores,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE type = 'album.restored' AND resource_id = ?",
    )
    .bind(album_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(restores, 1);
    let (status, back) = h
        .json(
            Request::get("/api/v1/search?q=coastal")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(back["items"]["albums"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == album_id));
}

#[tokio::test]
async fn access_change_is_audited_and_old_rows_expire() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Audit","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let before = album["revision"].as_i64().unwrap();
    let acl_before = album["acl_revision"].as_i64().unwrap();
    let (status, changed) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": before,
                        "visibility": "public",
                        "archive_enabled": false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["acl_revision"].as_i64().unwrap(), acl_before + 1);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let detail: (String,) = sqlx::query_as(
        "SELECT detail FROM audit_log WHERE action = 'album.access' AND target = ?",
    )
    .bind(album_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&detail.0).unwrap();
    assert_eq!(parsed["revision_before"], before);
    assert_eq!(parsed["revision_after"], changed["revision"]);
    assert!(parsed["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field == "visibility"));
    assert!(!detail.0.contains("password"));
    let (status, headers, body) = h
        .call(Request::get("/api/v1/admin/audit.jsonl").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/x-ndjson"
    );
    let text = String::from_utf8(body).unwrap();
    assert!(text.lines().any(|line| line.contains("album.access") && line.contains(album_id)));
    sqlx::query(
        "INSERT INTO audit_log(action, result, detail, occurred_at)
         VALUES('album.access', 'ok', '{\"fields\":[\"old\"]}', ?)",
    )
    .bind(gallery::db::now_ms() - gallery::events::AUDIT_RETAIN_MS - 1_000)
    .execute(&pool)
    .await
    .unwrap();
    gallery::events::prune_audit(&pool).await.unwrap();
    let (old,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM audit_log WHERE detail LIKE '%\"old\"%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old, 0);
    let (recent,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'album.access' AND target = ?",
    )
    .bind(album_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recent, 1);
    let (status, user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"auditor","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{user}");
    login_as(&mut h, "auditor", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "auditor", "collab-pass-13").await;
    let (status, _, _) = h
        .call(Request::get("/api/v1/admin/audit.jsonl").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unknown_json_field_is_rejected_and_stale_revision_asks_for_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, unknown) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Strict","description":"","extra":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(unknown["code"], "JSON_BODY");
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Strict","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{album}");
    let (status, stale) = h
        .json(
            Request::patch(format!("/api/v1/albums/{}", album["id"].as_str().unwrap()))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Renamed","expected_revision":0}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale["code"], "REVISION_CONFLICT");
    assert_eq!(stale["refresh"], true);
}

#[tokio::test]
async fn an_abandoned_upload_expires_and_drops_its_staged_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let album = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum {
            name: "Hold".into(),
            description: None,
        },
    )
    .await
    .unwrap();
    let batch = gallery::upload::create_batch(
        &state.pool,
        &actor,
        &album.id,
        gallery::upload::CreateBatch {
            items: vec![
                gallery::upload::BatchItemIn {
                    client_item_id: "old".into(),
                    name: "old.png".into(),
                    size: 4,
                },
                gallery::upload::BatchItemIn {
                    client_item_id: "fresh".into(),
                    name: "fresh.png".into(),
                    size: 4,
                },
            ],
        },
    )
    .await
    .unwrap();
    let old_id = &batch.items[0].upload_item_id;
    let fresh_id = &batch.items[1].upload_item_id;
    gallery::upload::begin_receive(&state.pool, &actor, &batch.id, old_id)
        .await
        .unwrap();
    gallery::upload::begin_receive(&state.pool, &actor, &batch.id, fresh_id)
        .await
        .unwrap();
    let staged: (String,) = sqlx::query_as("SELECT staged_key FROM upload_items WHERE id = ?")
        .bind(old_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let path = state.config.media_root().join(&staged.0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"partial").unwrap();
    let stale = gallery::db::now_ms() - gallery::upload::UPLOAD_HOLD_MS - 1_000;
    sqlx::query("UPDATE upload_items SET updated_at = ? WHERE id = ?")
        .bind(stale)
        .bind(old_id)
        .execute(&state.pool)
        .await
        .unwrap();
    let expired = gallery::upload::reclaim_abandoned_uploads(&state.pool, &state.storage)
        .await
        .unwrap();
    assert_eq!(expired, 1);
    assert!(!path.exists());
    let old: (String, Option<String>) =
        sqlx::query_as("SELECT state, failure_code FROM upload_items WHERE id = ?")
            .bind(old_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(old.0, "failed");
    assert_eq!(old.1.as_deref(), Some("UPLOAD_EXPIRED"));
    let fresh: (String,) = sqlx::query_as("SELECT state FROM upload_items WHERE id = ?")
        .bind(fresh_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(fresh.0, "receiving");
}

#[tokio::test]
async fn a_second_receive_while_the_claim_is_live_returns_the_batch_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let album = gallery::album::create(
        &state.pool,
        &actor,
        gallery::album::CreateAlbum {
            name: "Claim".into(),
            description: None,
        },
    )
    .await
    .unwrap();
    let batch = gallery::upload::create_batch(
        &state.pool,
        &actor,
        &album.id,
        gallery::upload::CreateBatch {
            items: vec![gallery::upload::BatchItemIn {
                client_item_id: "a".into(),
                name: "a.png".into(),
                size: 4,
            }],
        },
    )
    .await
    .unwrap();
    let item_id = &batch.items[0].upload_item_id;
    gallery::upload::begin_receive(&state.pool, &actor, &batch.id, item_id)
        .await
        .unwrap();
    let busy = gallery::upload::begin_receive(&state.pool, &actor, &batch.id, item_id)
        .await
        .unwrap_err();
    let text = busy.to_string();
    assert!(text.contains("UPLOAD_IN_PROGRESS"), "{text}");
    assert!(text.contains(&format!("/api/v1/uploads/{}", batch.id)), "{text}");
    let stale = gallery::db::now_ms() - gallery::upload::UPLOAD_CLAIM_MS - 1_000;
    sqlx::query("UPDATE upload_items SET updated_at = ? WHERE id = ?")
        .bind(stale)
        .bind(item_id)
        .execute(&state.pool)
        .await
        .unwrap();
    gallery::upload::begin_receive(&state.pool, &actor, &batch.id, item_id)
        .await
        .unwrap();
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, staged_key FROM upload_items WHERE id = ?")
            .bind(item_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(row.0, "receiving");
    assert!(row.1.as_ref().is_some_and(|key| !key.is_empty()));
}

#[tokio::test]
async fn an_interrupted_upload_keeps_the_receiving_row_and_staged_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Hold"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{album}");
    let album_id = album["id"].as_str().unwrap();
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"items":[{"client_item_id":"a","name":"a.png","size":8}]}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{batch}");
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, problem) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{problem}");
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, staged_key FROM upload_items WHERE id = ?")
            .bind(item_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, "receiving");
    assert!(row.1.as_ref().is_some_and(|key| !key.is_empty()));
    let images: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(images.0, 0);
}

#[tokio::test]
async fn upload_body_is_refused_before_an_anonymous_file_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Auth first"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size":png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{batch}");
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let url = format!("/api/v1/uploads/{batch_id}/items/{item_id}/body");
    h.cookie.clear();
    h.csrf.clear();
    let (status, rejected) = h
        .json(
            Request::put(&url)
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(rejected["code"], "UNAUTHENTICATED");
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (item_state,): (String,) =
        sqlx::query_as("SELECT state FROM upload_items WHERE id = ?")
            .bind(item_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(item_state, "waiting");
    h.login().await;
    let (status, committed) = h
        .json(
            Request::put(&url)
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{committed}");
    assert!(committed["image_id"].as_str().is_some());
}

#[tokio::test]
async fn json_writes_are_capped_and_image_uploads_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, too_big) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, "2000000")
                .body(Body::from(r#"{"name":"Too big","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(too_big["code"], "JSON_TOO_LARGE");
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Sized","description":""}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{album}");
    let album_id = album["id"].as_str().unwrap();
    let png = png_bytes(8, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id":"a","name":"a.png","size":png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{batch}");
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, committed) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .header(header::CONTENT_LENGTH, "2000000")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{committed}");
}

#[tokio::test]
async fn health_and_cancel_are_not_recorded_as_faults() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, _, _) = h
        .call(Request::get("/health/live").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .call(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    let media = dir.path().join("media");
    let parked = dir.path().join("media-parked");
    std::fs::rename(&media, &parked).unwrap();
    let (status, _, _) = h
        .call(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    std::fs::rename(&parked, &media).unwrap();
    let now = gallery::db::now_ms();
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, available_at, created_at, updated_at)
         VALUES('job-cancel-fault', 'backup', 'cancel-fault', '{}', '{}', 'queued', ?, ?, ?)",
    )
    .bind(now + 3_600_000)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let (status, canceled) = h
        .json(
            Request::post("/api/v1/jobs/job-cancel-fault/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{canceled}");
    assert_eq!(canceled["state"], "canceled");
    let (faults,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM request_faults")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(faults, 0);
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Fault"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "fault.png", 16).await;
    let (key,): (String,) = sqlx::query_as(
        "SELECT l.object_key FROM media_locations l
         JOIN images i ON i.original_blob_id = l.blob_id WHERE i.id = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let path = dir.path().join("media").join(&key);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let (status, blip) = h
        .json(
            Request::get(format!(
                "/api/v1/albums/{album_id}/images/{image_id}/original-download"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(blip["code"], "STORAGE_UNAVAILABLE");
    let (status_code,): (i64,) = sqlx::query_as("SELECT status FROM request_faults")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status_code, 503);
}

#[tokio::test]
async fn events_older_than_a_week_or_past_the_cap_force_resync() {
    let dir = tempfile::tempdir().unwrap();
    let pool = gallery::db::connect(&dir.path().join("gallery.sqlite"))
        .await
        .unwrap();
    let now = gallery::db::now_ms();
    for (name, at) in [
        ("old", now - gallery::events::EVENT_RETAIN_MS - 5_000),
        ("kept", now),
    ] {
        sqlx::query(
            "INSERT INTO events(type, scope_kind, scope_id, resource_id, payload_json, occurred_at)
             VALUES('album.created', 'album', ?, ?, '{}', ?)",
        )
        .bind(name)
        .bind(name)
        .bind(at)
        .execute(&pool)
        .await
        .unwrap();
    }
    let removed = gallery::events::retain_events(&pool).await.unwrap();
    assert_eq!(removed, 1);
    let (left,): (String,) = sqlx::query_as("SELECT resource_id FROM events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, "kept");
    let stale = gallery::events::feed(&pool, Some(0), None, true, 10)
        .await
        .unwrap();
    assert!(stale.stale);
    for index in 0..4 {
        sqlx::query(
            "INSERT INTO events(type, scope_kind, resource_id, payload_json, occurred_at)
             VALUES('album.created', 'album', ?, '{}', ?)",
        )
        .bind(format!("n{index}"))
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }
    gallery::events::retain_events_limited(&pool, gallery::events::EVENT_RETAIN_MS, 2)
        .await
        .unwrap();
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let newest: Vec<String> = sqlx::query_as(
        "SELECT resource_id FROM events ORDER BY id ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(id,): (String,)| id)
    .collect();
    assert_eq!(newest, vec!["n2".to_string(), "n3".to_string()]);
}

#[tokio::test]
async fn event_stream_closes_when_the_session_is_revoked_mid_stream() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let mut request = Request::get("/api/v1/events").body(Body::empty()).unwrap();
    request.headers_mut().insert(
        header::COOKIE,
        format!("gallery_session={}", h.cookie).parse().unwrap(),
    );
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (session_id,): (String,) = sqlx::query_as(
        "SELECT id FROM sessions WHERE revoked_at IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    gallery::auth::revoke_session(&pool, &session_id).await.unwrap();
    let ended = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        response.into_body().collect(),
    )
    .await;
    assert!(ended.is_ok(), "the event stream should end after the session is revoked");
}

#[tokio::test]
async fn event_stream_is_not_buffered_or_cached() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let mut request = Request::get("/api/v1/events").body(Body::empty()).unwrap();
    request.headers_mut().insert(
        header::COOKIE,
        format!("gallery_session={}", h.cookie).parse().unwrap(),
    );
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_type.starts_with("text/event-stream"));
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache, no-transform"
    );
    assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");
}

#[tokio::test]
async fn event_history_can_be_polled_when_the_stream_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, snap) = h
        .json(
            Request::get("/api/v1/events?snapshot=1")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snap["stale"], false);
    assert!(snap["events"].as_array().unwrap().is_empty());
    let cursor = snap["event_cursor"].as_i64().unwrap();
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Polled"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let (status, page) = h
        .json(
            Request::get(format!("/api/v1/events?snapshot=1&after={cursor}"))
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["stale"], false);
    let event_id = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "album.created" && event["resource_id"] == album_id)
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    gallery::events::prune_through(&pool, event_id).await.unwrap();
    let (status, stale) = h
        .json(
            Request::get(format!("/api/v1/events?snapshot=1&after={cursor}"))
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stale["stale"], true);
    assert!(stale["events"].as_array().unwrap().is_empty());
    assert!(stale["event_cursor"].as_i64().unwrap() >= event_id);
}

#[tokio::test]
async fn event_feed_shrinks_when_the_album_grant_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Granted"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let revision = album["revision"].as_i64().unwrap();
    let (status, user) = h
        .json(
            Request::post("/api/v1/admin/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"username":"reader","password":"collab-pass-12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{user}");
    let reader = user["id"].as_str().unwrap();
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": revision,
                        "grants": [{"actor_id": reader, "browse": true}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    login_as(&mut h, "reader", "collab-pass-12").await;
    let (status, _, _) = h
        .call(
            Request::patch("/api/v1/account/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"current_password":"collab-pass-12","new_password":"collab-pass-13","confirm_password":"collab-pass-13"}"#,
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    login_as(&mut h, "reader", "collab-pass-13").await;
    let (status, seen) = h
        .json(
            Request::get("/api/v1/events?snapshot=1&after=0")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(seen["events"].as_array().unwrap().iter().any(|event| {
        event["type"] == "album.created" && event["resource_id"] == album_id
    }));
    h.login().await;
    let (status, current) = h
        .json(
            Request::get(format!("/api/v1/albums/{album_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .json(
            Request::patch(format!("/api/v1/albums/{album_id}/access"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "expected_revision": current["revision"],
                        "grants": [{"actor_id": reader, "browse": false}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    login_as(&mut h, "reader", "collab-pass-13").await;
    let (status, hidden) = h
        .json(
            Request::get("/api/v1/events?snapshot=1&after=0")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(hidden["events"].as_array().unwrap().iter().all(|event| {
        !(event["resource_id"] == album_id || event["scope_id"] == album_id)
    }));
}

#[tokio::test]
async fn event_poll_stops_after_the_session_is_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let (session_id,): (String,) = sqlx::query_as(
        "SELECT id FROM sessions WHERE revoked_at IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(gallery::auth::session_open(&pool, &session_id).await.unwrap());
    let (status, _) = h
        .json(
            Request::get("/api/v1/events?snapshot=1")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .json(Request::delete("/api/v1/session").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!gallery::auth::session_open(&pool, &session_id).await.unwrap());
    let (status, denied) = h
        .json(
            Request::get("/api/v1/events?snapshot=1")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(denied["code"], "UNAUTHENTICATED");
}

#[tokio::test]
async fn trashing_an_image_records_the_lifecycle_with_the_state_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path().to_path_buf()).await;
    h.login().await;
    let (status, album) = h
        .json(
            Request::post("/api/v1/albums")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Lifecycle"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let album_id = album["id"].as_str().unwrap();
    let image_id = upload_png(&h, album_id, "life.png", 12).await;
    let (status, trashed) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/trash"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"expected_lifecycle_revision":1}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{trashed}");
    assert_eq!(trashed["state"], "trashed");
    let pool = gallery::db::connect(&dir.path().join("db").join("gallery.sqlite"))
        .await
        .unwrap();
    let detail: (String,) = sqlx::query_as(
        "SELECT detail FROM audit_log WHERE action = 'image.lifecycle' AND target = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&detail.0).unwrap();
    assert_eq!(parsed["from"], "active");
    assert_eq!(parsed["to"], "trashed");
    assert_eq!(parsed["lifecycle_before"], 1);
    assert_eq!(parsed["lifecycle_after"], trashed["lifecycle_revision"]);
    let (events,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE type = 'image.lifecycle' AND resource_id = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    let (status, restored) = h
        .json(
            Request::post(format!("/api/v1/images/{image_id}/restore"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    format!(
                        r#"{{"expected_lifecycle_revision":{}}}"#,
                        trashed["lifecycle_revision"]
                    ),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["state"], "active");
    let (audits,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'image.lifecycle' AND target = ?",
    )
    .bind(&image_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 2);
}

fn urlencoding_cursor(cursor: &str) -> String {
    cursor.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

async fn upload_png(h: &Harness, album_id: &str, name: &str, width: u32) -> String {
    let png = png_bytes(width, 8, false);
    let (status, batch) = h
        .json(
            Request::post(format!("/api/v1/albums/{album_id}/uploads"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"items":[{"client_item_id": name, "name": name, "size": png.len()}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{batch}");
    let batch_id = batch["id"].as_str().unwrap();
    let item_id = batch["items"][0]["upload_item_id"].as_str().unwrap();
    let (status, commit) = h
        .json(
            Request::put(format!("/api/v1/uploads/{batch_id}/items/{item_id}/body"))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(png))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{commit}");
    commit["image_id"].as_str().unwrap().to_string()
}

#[allow(dead_code)]
fn _addr() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}
