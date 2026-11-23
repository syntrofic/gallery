use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{code}: {detail}")]
    Problem {
        status: StatusCode,
        code: &'static str,
        title: &'static str,
        detail: String,
        retryable: bool,
        retry_after_secs: Option<u64>,
        refresh: bool,
    },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    pub fn new(
        status: StatusCode,
        code: &'static str,
        title: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self::Problem {
            status,
            code,
            title,
            detail: detail.into(),
            retryable: status == StatusCode::SERVICE_UNAVAILABLE
                || status == StatusCode::TOO_MANY_REQUESTS,
            retry_after_secs: None,
            refresh: false,
        }
    }

    pub fn rate_limited(retry_after_secs: u64) -> Self {
        Self::Problem {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "RATE_LIMIT",
            title: "Too Many Requests",
            detail: "try again later".into(),
            retryable: true,
            retry_after_secs: Some(retry_after_secs.max(1)),
            refresh: false,
        }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", "Not Found", "resource not found")
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "Unauthorized",
            "authentication required",
        )
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "FORBIDDEN", "Forbidden", "not permitted")
    }

    pub fn conflict(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, "Conflict", detail)
    }

    pub fn precondition(code: &'static str, detail: impl Into<String>) -> Self {
        let mut err = Self::new(StatusCode::PRECONDITION_FAILED, code, "Precondition Failed", detail);
        if let Self::Problem { refresh, .. } = &mut err {
            *refresh = true;
        }
        err
    }

    pub fn unprocessable(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, "Unprocessable Entity", detail)
    }

    pub fn bad_request(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, "Bad Request", detail)
    }

    pub fn gated(feature: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "FEATURE_GATED",
            "Feature gated",
            format!("{feature} is not enabled"),
        )
    }
}

#[derive(Serialize)]
struct ProblemBody<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    title: &'a str,
    status: u16,
    detail: &'a str,
    code: &'a str,
    request_id: &'a str,
    retryable: bool,
    #[serde(skip_serializing_if = "skip_false")]
    refresh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
}

fn skip_false(value: &bool) -> bool {
    !*value
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let request_id = crate::http_util::current_request_id();
        let (status, code, title, detail, retryable, retry_after, refresh) = match &self {
            AppError::Problem {
                status,
                code,
                title,
                detail,
                retryable,
                retry_after_secs,
                refresh,
            } => (
                *status,
                *code,
                *title,
                detail.clone(),
                *retryable,
                *retry_after_secs,
                *refresh,
            ),
            AppError::Internal(err) => {
                tracing::error!(error = %err, "internal error");
                #[cfg(debug_assertions)]
                eprintln!("internal: {err:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    "Internal Server Error",
                    "internal error".to_string(),
                    false,
                    None,
                    false,
                )
            }
        };
        let body = ProblemBody {
            kind: "about:blank",
            title,
            status: status.as_u16(),
            detail: &detail,
            code,
            request_id: &request_id,
            retryable,
            refresh,
            retry_after_ms: retry_after
                .map(|secs| secs.saturating_mul(1000))
                .or(retryable.then_some(5_000)),
        };
        let mut response = (status, axum::Json(body)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&request_id).unwrap_or(HeaderValue::from_static("unknown")),
        );
        let retry_secs = retry_after.or(retryable.then_some(5));
        if let Some(secs) = retry_secs {
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

pub type AppResult<T> = Result<T, AppError>;
