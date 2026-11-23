use std::cell::RefCell;

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

tokio::task_local! {
    static REQUEST_ID: RefCell<String>;
}

pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(|id| id.borrow().clone())
        .unwrap_or_else(|_| "unbound".into())
}

pub async fn request_id_mw(mut req: Request, next: Next) -> Response {
    let rid = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    req.extensions_mut().insert(rid.clone());
    REQUEST_ID
        .scope(RefCell::new(rid.clone()), async move {
            let mut res = next.run(req).await;
            if let Ok(val) = HeaderValue::from_str(&rid) {
                res.headers_mut().insert("x-request-id", val);
            }
            res
        })
        .await
}

pub fn cookie_header(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let mut parts = vec![
        format!("{name}={value}"),
        "Path=/".into(),
        "HttpOnly".into(),
        "SameSite=Lax".into(),
        format!("Max-Age={max_age}"),
    ];
    if secure {
        parts.push("Secure".into());
    }
    parts.join("; ")
}

pub fn parse_cookie<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?
        .split(';')
        .map(|p| p.trim())
        .find_map(|p| p.strip_prefix(&format!("{name}=")))
}

pub fn json_reject_unknown<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, crate::error::AppError> {
    serde_json::from_value::<T>(value).map_err(|err| {
        crate::error::AppError::bad_request("INVALID_JSON", err.to_string())
    })
}

pub fn header_origin(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

pub fn content_disposition(name: &str) -> String {
    let mut cleaned = String::new();
    for ch in name.chars() {
        if ch.is_control() || ch == '/' || ch == '\\' || ch == '"' {
            continue;
        }
        cleaned.push(ch);
    }
    let cleaned = cleaned.trim().trim_matches('.');
    let cleaned = if cleaned.is_empty() { "download" } else { cleaned };
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && !ext.contains('.') => (stem, format!(".{ext}")),
        _ => (cleaned, String::new()),
    };
    let stem = if reserved_download_name(stem) {
        format!("_{stem}")
    } else {
        stem.to_string()
    };
    let safe = format!("{stem}{ext}");
    let encoded = percent_encode_utf8(&safe);
    let ascii: String = safe
        .chars()
        .map(|ch| if ch.is_ascii() && ch != '"' { ch } else { '_' })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

fn reserved_download_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit())
}

fn percent_encode_utf8(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub const READ_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
pub enum ReadFault {
    Idle,
    TooLarge,
    Closed,
}

pub fn stream_open_file(
    file: tokio::fs::File,
    max_bytes: Option<u64>,
    live: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
) -> axum::body::Body {
    if let Some(counter) = &live {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    let guard = StreamHold(live);
    let stream = futures_util::stream::unfold(
        (file, guard, max_bytes),
        |(mut file, guard, mut remaining)| async move {
            if remaining == Some(0) {
                return None;
            }
            let mut buf = vec![0u8; 64 * 1024];
            let want = match remaining {
                Some(left) => buf.len().min(left as usize),
                None => buf.len(),
            };
            let read = tokio::io::AsyncReadExt::read(&mut file, &mut buf[..want]).await;
            match read {
                Ok(0) => None,
                Ok(n) => {
                    if let Some(left) = remaining.as_mut() {
                        *left = left.saturating_sub(n as u64);
                    }
                    buf.truncate(n);
                    Some((
                        Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(buf)),
                        (file, guard, remaining),
                    ))
                }
                Err(err) => Some((Err(err), (file, guard, Some(0)))),
            }
        },
    );
    axum::body::Body::from_stream(stream)
}

struct StreamHold(Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>);

impl Drop for StreamHold {
    fn drop(&mut self) {
        if let Some(counter) = &self.0 {
            counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

pub async fn read_with_idle(
    body: axum::body::Body,
    limit: usize,
    idle: std::time::Duration,
) -> Result<bytes::Bytes, ReadFault> {
    use http_body_util::BodyExt;
    let mut body = std::pin::pin!(body);
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(idle, body.as_mut().frame()).await {
            Err(_) => return Err(ReadFault::Idle),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(ReadFault::Closed),
            Ok(Some(Ok(frame))) => {
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                if out.len().saturating_add(data.len()) > limit {
                    return Err(ReadFault::TooLarge);
                }
                out.extend_from_slice(&data);
            }
        }
    }
    Ok(bytes::Bytes::from(out))
}

pub fn origin_allowed(origin: &str, public_url: &str) -> bool {
    let origin = origin.trim_end_matches('/');
    let public_url = public_url.trim_end_matches('/');
    origin.eq_ignore_ascii_case(public_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upload_read_stops_when_the_client_goes_idle() {
        let body = axum::body::Body::from_stream(futures_util::stream::unfold(0u8, |step| async move {
            if step == 0 {
                Some((
                    Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from_static(b"ab")),
                    1,
                ))
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                None
            }
        }));
        let started = std::time::Instant::now();
        let err = read_with_idle(body, 1024, std::time::Duration::from_millis(40))
            .await
            .unwrap_err();
        assert!(matches!(err, ReadFault::Idle));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let ready = read_with_idle(
            axum::body::Body::from(bytes::Bytes::from_static(b"png")),
            1024,
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(&ready[..], b"png");
    }

    #[tokio::test]
    async fn dropping_a_file_stream_releases_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.bin");
        std::fs::write(&path, vec![7u8; 200_000]).unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let body = stream_open_file(file, Some(200_000), Some(live.clone()));
        assert_eq!(live.load(std::sync::atomic::Ordering::SeqCst), 1);
        {
            let mut body = std::pin::pin!(body);
            use http_body_util::BodyExt;
            let frame = body.as_mut().frame().await.unwrap().unwrap();
            assert!(!frame.into_data().unwrap().is_empty());
        }
        assert_eq!(live.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
