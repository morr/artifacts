//! HTTP API.
//!
//! Public (no token): `GET /a/{name}` and `GET /healthz`.
//! Everything under `/api/` requires `Authorization: Bearer <ARTIFACTS_TOKEN>`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{header, request::Parts, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::store::{FileStore, PutOutcome, NAME_RULE};

pub struct AppState {
    pub store: FileStore,
    pub token: Vec<u8>,
    pub public_base: String,
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState, max_body: usize) -> Router {
    Router::new()
        .route("/a/{name}", get(get_public))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/artifacts", get(list_artifacts))
        .route("/api/artifacts/{name}", get(get_meta).put(put_artifact).delete(delete_artifact))
        .route("/api/artifacts/{name}/versions/{version}", get(get_version))
        .route("/api/artifacts/{name}/rollback/{version}", post(rollback))
        .fallback(not_found)
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .with_state(state)
}

// --- auth ---

struct Authed;

impl FromRequestParts<SharedState> for Authed {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &SharedState) -> Result<Self, Self::Rejection> {
        let provided = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::as_bytes)
            .unwrap_or_default();
        if provided.len() == state.token.len() && provided.ct_eq(&state.token).into() {
            Ok(Authed)
        } else {
            Err(error_response(StatusCode::UNAUTHORIZED, "invalid or missing bearer token"))
        }
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message });
    (status, Json(body)).into_response()
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
}

// --- validation ---

fn bad_name(name: &str) -> Option<Response> {
    if crate::store::valid_name(name) {
        None
    } else {
        Some(error_response(StatusCode::BAD_REQUEST, NAME_RULE))
    }
}

/// Version ids are fixed-width timestamps like 2026-10-09T18-22-33-123Z.
fn bad_version(version: &str) -> Option<Response> {
    let ok = version.len() == 24
        && version.ends_with('Z')
        && version[..23].bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b'T');
    if ok { None } else { Some(error_response(StatusCode::BAD_REQUEST, "invalid version id")) }
}

// --- public routes ---

async fn get_public(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    if crate::store::valid_name(&name) {
        match state.store.get_current(&name) {
            Ok(Some((content, meta))) => build_content_response(content, &meta.content_type),
            Ok(None) => not_found().await,
            Err(e) => storage_error(e),
        }
    } else {
        // Invalid names are indistinguishable from missing ones to the public.
        not_found().await
    }
}

fn build_content_response(content: Vec<u8>, content_type: &str) -> Response {
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, sanitize_content_type(content_type))
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(len) = HeaderValue::from_str(&content.len().to_string()) {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder.body(Body::from(content)).unwrap()
}

/// Header values must not contain CR/LF; meta comes from our own writes,
/// but the content type originates from a request header, so check anyway.
fn sanitize_content_type(ct: &str) -> HeaderValue {
    let clean: String = ct.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    HeaderValue::from_str(&clean).unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

fn storage_error(e: std::io::Error) -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("storage error: {e}"))
}

// --- api routes ---

#[derive(Deserialize)]
struct ListParams {
    prefix: Option<String>,
}

async fn list_artifacts(State(state): State<SharedState>, _auth: Authed, Query(params): Query<ListParams>) -> Response {
    match state.store.list(params.prefix.as_deref().unwrap_or("")) {
        Ok(items) => Json(items).into_response(),
        Err(e) => storage_error(e),
    }
}

async fn get_meta(State(state): State<SharedState>, _auth: Authed, Path(name): Path<String>) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    match state.store.get_meta(&name) {
        Ok(Some((meta, versions))) => {
            let body = serde_json::json!({
                "name": name,
                "url": format!("{}/a/{}", state.public_base.trim_end_matches('/'), name),
                "meta": meta,
                "versions": versions,
            });
            Json(body).into_response()
        }
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(e) => storage_error(e),
    }
}

async fn put_artifact(
    State(state): State<SharedState>,
    _auth: Authed,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    let create_only = headers
        .get("x-create-only")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let update_only = headers
        .get("x-update-only")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    if create_only && update_only {
        return error_response(StatusCode::BAD_REQUEST, "x-create-only and x-update-only are mutually exclusive");
    }

    let exists = state.store.exists(&name);
    if create_only && exists {
        return error_response(StatusCode::CONFLICT, "artifact already exists");
    }
    if update_only && !exists {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/plain; charset=utf-8")
        .to_string();

    match state.store.put(&name, &body, &content_type) {
        Ok(PutOutcome::Created) => (
            StatusCode::CREATED,
            Json(put_body(&state, &name, true)),
        )
            .into_response(),
        Ok(PutOutcome::Updated | PutOutcome::Unchanged) => {
            Json(put_body(&state, &name, false)).into_response()
        }
        Err(e) => storage_error(e),
    }
}

fn put_body(state: &SharedState, name: &str, created: bool) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "url": format!("{}/a/{}", state.public_base.trim_end_matches('/'), name),
        "created": created,
    })
}

async fn get_version(
    State(state): State<SharedState>,
    _auth: Authed,
    Path((name, version)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    if let Some(resp) = bad_version(&version) {
        return resp;
    }
    match state.store.get_version(&name, &version) {
        Ok(Some((content, info))) => build_content_response(content, &info.content_type),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "version not found"),
        Err(e) => storage_error(e),
    }
}

async fn rollback(
    State(state): State<SharedState>,
    _auth: Authed,
    Path((name, version)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    if let Some(resp) = bad_version(&version) {
        return resp;
    }
    match state.store.rollback(&name, &version) {
        Ok(Some(meta)) => (StatusCode::OK, Json(meta)).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact or version not found"),
        Err(e) => storage_error(e),
    }
}

async fn delete_artifact(State(state): State<SharedState>, _auth: Authed, Path(name): Path<String>) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    match state.store.delete(&name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(e) => storage_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state(dir: &std::path::Path) -> SharedState {
        Arc::new(AppState {
            store: FileStore::new(dir).unwrap(),
            token: b"secret-token".to_vec(),
            public_base: "https://artifacts.example.com".to_string(),
        })
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>, Option<String>) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, body, content_type)
    }

    fn auth_put(name: &str, body: &'static str, extra: &[(&'static str, &'static str)]) -> Request<Body> {
        let mut builder = Request::put(format!("/api/artifacts/{name}"))
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8");
        for (k, v) in extra {
            builder = builder.header(*k, *v);
        }
        builder.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn api_requires_token() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        for (method, path) in [
            (axum::http::Method::GET, "/api/artifacts"),
            (axum::http::Method::GET, "/api/artifacts/x-report"),
            (axum::http::Method::PUT, "/api/artifacts/x-report"),
            (axum::http::Method::DELETE, "/api/artifacts/x-report"),
        ] {
            let req = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
            let (status, _, _) = send(&app, req).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        }
        // wrong token is rejected too
        let req = Request::get("/api/artifacts")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn public_get_and_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // public 404 before it exists
        let (status, _, _) = send(&app, Request::get("/a/x-report").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // create-only
        let (status, body, _) = send(&app, auth_put("x-report", "<h1>one</h1>", &[("x-create-only", "true")])).await;
        assert_eq!(status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&body));

        // second create-only conflicts
        let (status, _, _) = send(&app, auth_put("x-report", "<h1>two</h1>", &[("x-create-only", "true")])).await;
        assert_eq!(status, StatusCode::CONFLICT);

        // update
        let (status, _, _) = send(&app, auth_put("x-report", "<h1>two</h1>", &[])).await;
        assert_eq!(status, StatusCode::OK);

        // public get serves content with stored content type
        let (status, body, ct) = send(&app, Request::get("/a/x-report").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"<h1>two</h1>");
        assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));

        // meta lists one archived version, rollback restores it
        let req = Request::get("/api/artifacts/x-report")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["url"], "https://artifacts.example.com/a/x-report");
        let versions = meta["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        let first = versions[0]["version"].as_str().unwrap().to_string();

        let req = Request::post(format!("/api/artifacts/x-report/rollback/{first}"))
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let (_, body, _) = send(&app, Request::get("/a/x-report").body(Body::empty()).unwrap()).await;
        assert_eq!(body, b"<h1>one</h1>");

        // delete, then public 404 again
        let req = Request::delete("/api/artifacts/x-report")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = send(&app, Request::get("/a/x-report").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_only_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // update-only on a missing artifact
        let (status, _, _) = send(&app, auth_put("x-new", "x", &[("x-update-only", "true")])).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // invalid names: 400 on the API, plain 404 on the public route
        let req = Request::put("/api/artifacts/Bad_Name")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::from("x"))
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = send(&app, Request::get("/a/Bad_Name").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // unknown paths never leak that this is an artifact store
        let (status, _, _) = send(&app, Request::get("/").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(&app, Request::get("/api").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn body_limit_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 16);
        let req = Request::put("/api/artifacts/x-big")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::from(vec![b'x'; 1024]))
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
