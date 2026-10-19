//! HTTP API.
//!
//! Public (no token): `GET /a/{name}`, `GET /i/{name}`, `GET /a/{name}/live`
//! and `GET /healthz`.
//! Everything under `/api/` requires `Authorization: Bearer <ARTIFACTS_TOKEN>`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{header, request::Parts, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use subtle::ConstantTimeEq;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;

use crate::store::{FileStore, PutOutcome, RenameOutcome, NAME_RULE};

pub struct AppState {
    pub store: FileStore,
    pub images: FileStore,
    pub token: Vec<u8>,
    pub public_base: String,
    /// Mutation notifications fanned out to `/a/{name}/live` streams.
    pub events: tokio::sync::broadcast::Sender<ArtifactEvent>,
}

pub type SharedState = Arc<AppState>;

/// A change to one artifact, as seen by open `/a/{name}/live` streams.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ArtifactEvent {
    Updated { name: String },
    Renamed { name: String, to: String },
}

impl ArtifactEvent {
    /// The name a stream must be watching to receive this event.
    fn about(&self) -> &str {
        match self {
            ArtifactEvent::Updated { name } | ArtifactEvent::Renamed { name, .. } => name,
        }
    }

    fn sse(&self) -> SseEvent {
        let kind = match self {
            ArtifactEvent::Updated { .. } => "updated",
            ArtifactEvent::Renamed { .. } => "renamed",
        };
        SseEvent::default()
            .event(kind)
            .data(serde_json::to_string(self).unwrap_or_default())
    }
}

pub fn router(state: SharedState, max_body: usize) -> Router {
    Router::new()
        .route("/a/{name}", get(get_public))
        .route("/a/{name}/live", get(live_artifact))
        .route("/i/{name}", get(get_image_public))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/artifacts", get(list_artifacts))
        .route("/api/artifacts/{name}", get(get_meta).put(put_artifact).delete(delete_artifact))
        .route("/api/artifacts/{name}/content", get(get_artifact_content))
        .route("/api/artifacts/{name}/versions/{version}", get(get_version))
        .route("/api/artifacts/{name}/rollback/{version}", post(rollback))
        .route("/api/artifacts/{name}/rename/{new_name}", post(rename_artifact))
        .route("/api/images", get(list_images))
        .route("/api/images/{name}", get(get_image_meta).put(put_image).delete(delete_image))
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

/// Public URL of an artifact (`kind = "a"`) or an image (`kind = "i"`).
fn public_url(state: &SharedState, kind: &str, name: &str) -> String {
    format!("{}/{}/{}", state.public_base.trim_end_matches('/'), kind, name)
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
    if !crate::store::valid_name(&name) {
        // Invalid names are indistinguishable from missing ones to the public.
        return not_found().await;
    }
    match state.store.get_current(&name) {
        Ok(Some((content, meta))) => {
            let content = if meta.content_type.starts_with("text/html") {
                inject_live_script(content, &name)
            } else {
                content
            };
            build_content_response(content, &meta.content_type)
        }
        Ok(None) => match state.store.resolve_alias(&name) {
            // The artifact was renamed away: the old URL redirects while the
            // artifact still exists under the new name.
            Ok(Some(target)) if state.store.exists(&target) => redirect_response(&state, &target),
            _ => not_found().await,
        },
        Err(e) => storage_error(e),
    }
}

/// Server-sent events about one artifact: `updated` after a put/rollback and
/// `renamed` when it moves to a new name. Public, like the page itself — it
/// leaks nothing the page doesn't. Opened on a name that was renamed away
/// earlier, the stream announces the new name immediately, so a stale tab
/// follows the artifact before any new mutation is needed.
async fn live_artifact(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    if !crate::store::valid_name(&name) {
        return not_found().await;
    }
    // Mirror the page route: a missing name with no live alias has nothing to
    // watch. A name renamed away keeps streaming — it announces the new name.
    let alias = match state.store.resolve_alias(&name) {
        Ok(Some(to)) if state.store.exists(&to) => Some(to),
        Ok(_) => None,
        Err(e) => return storage_error(e),
    };
    if !state.store.exists(&name) && alias.is_none() {
        return not_found().await;
    }
    let initial = alias.map(|to| ArtifactEvent::Renamed { name: name.clone(), to });
    let watched = name;
    let updates = BroadcastStream::new(state.events.subscribe()).filter_map(move |item| match item {
        Ok(event) => (event.about() == watched).then(|| Ok(event.sse())),
        // Fell behind and lost some events: a safe catch-up is a reload.
        Err(BroadcastStreamRecvError::Lagged(_)) => {
            Some(Ok(ArtifactEvent::Updated { name: watched.clone() }.sse()))
        }
    });
    let initial: Vec<Result<SseEvent, Infallible>> = initial.map(|ev| vec![Ok(ev.sse())]).unwrap_or_default();
    let stream = tokio_stream::iter(initial).chain(updates);

    let mut response = Sse::new(stream).keep_alive(KeepAlive::default()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

/// Client side of live reload, injected into served HTML: an EventSource on
/// `/a/{name}/live`, a scroll-preserving `location.reload()` on updates and
/// `location.replace()` to the new URL on renames, plus timestamped toasts
/// telling the reader the page changed. Toasts never fade on their own — each
/// click dismisses one — and stack, so a burst of updates reads as a log.
/// Messages accumulate in sessionStorage across reloads (per-tab, so tabs
/// don't steal each other's scroll), capped at the last 20.
const LIVE_SCRIPT: &str = r#"<script>
(function () {
  var name = "__NAME__";
  var prefix = "artifacts-live-scroll:";
  var key = prefix + name;
  function clock() {
    var d = new Date();
    function p(n) { return (n < 10 ? "0" : "") + n; }
    return p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
  }
  function box() {
    var c = document.getElementById("artifacts-live-notes");
    if (!c) {
      c = document.createElement("div");
      c.id = "artifacts-live-notes";
      c.style.cssText = "position:fixed;right:16px;bottom:16px;z-index:2147483647;"
        + "display:flex;flex-direction:column;gap:6px;align-items:flex-end;";
      document.body.appendChild(c);
    }
    return c;
  }
  function toast(text) {
    if (!document.body) return;
    var el = document.createElement("div");
    el.setAttribute("role", "status");
    el.textContent = text;
    el.title = "скрыть";
    el.style.cssText = "pointer-events:auto;cursor:pointer;"
      + "background:rgba(23,23,23,.8);color:#fafafa;font:500 12px/1.4 ui-sans-serif,system-ui,sans-serif;"
      + "padding:6px 10px;border-radius:8px";
    el.addEventListener("click", function () { el.remove(); });
    box().appendChild(el);
  }
  function restore() {
    var raw = null;
    try {
      raw = sessionStorage.getItem(key);
      sessionStorage.removeItem(key);
    } catch (e) {}
    if (raw === null) return;
    var info;
    try { info = JSON.parse(raw); } catch (e) { return; }
    var scroll = function () {
      if (typeof info.y === "number" && info.y > 0) window.scrollTo(0, info.y);
    };
    scroll();
    window.addEventListener("load", scroll);
    if (info.msg) toast(info.msg);
    if (Array.isArray(info.msgs)) info.msgs.forEach(function (m) { toast(m); });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", restore);
  else restore();
  function save(to, msg) {
    try {
      var info = {};
      try { info = JSON.parse(sessionStorage.getItem(prefix + to)) || {}; } catch (e) {}
      info.y = window.scrollY;
      if (!Array.isArray(info.msgs)) info.msgs = [];
      info.msgs.push(msg);
      if (info.msgs.length > 20) info.msgs = info.msgs.slice(-20);
      sessionStorage.setItem(prefix + to, JSON.stringify(info));
    } catch (e) {}
  }
  var source = new EventSource("/a/" + name + "/live");
  source.addEventListener("updated", function () {
    save(name, "обновлено в " + clock());
    location.reload();
  });
  source.addEventListener("renamed", function (ev) {
    var to = null;
    try { to = JSON.parse(ev.data).to; } catch (e) {}
    if (!to || to === name) return;
    save(to, "переименовано: " + to);
    location.replace("/a/" + to);
  });
})();
</script>"#;

/// Splice the live script in before the closing `</body>` (or `</html>`, or
/// append when there is neither). The search runs on a lowercased copy so
/// offset stays valid: the transform is ASCII-only, byte-for-byte.
fn inject_live_script(mut html: Vec<u8>, name: &str) -> Vec<u8> {
    let script = LIVE_SCRIPT.replace("__NAME__", name);
    let lower = html.to_ascii_lowercase();
    // The real closing tag is the last occurrence; earlier ones are content.
    let at = [b"</body>" as &[u8], b"</html>"]
        .iter()
        .find_map(|tag| lower.windows(tag.len()).rposition(|w| w == *tag));
    match at {
        Some(at) => {
            html.splice(at..at, script.bytes());
        }
        None => {
            html.push(b'\n');
            html.extend_from_slice(script.as_bytes());
        }
    }
    html
}

/// 302, not 301: names can be reused (delete + create), and a cached
/// permanent redirect would then serve the wrong artifact.
fn redirect_response(state: &SharedState, target: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, public_url(state, "a", target))
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .body(Body::empty())
        .unwrap()
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
                "url": public_url(&state, "a", &name),
                "meta": meta,
                "versions": versions,
            });
            Json(body).into_response()
        }
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(e) => storage_error(e),
    }
}

/// Raw stored bytes. Unlike the public `/a/{name}`, text/html is served
/// without the injected live-reload script — API consumers round-tripping
/// content (get_artifact → edit → put) need the bytes exactly as stored.
async fn get_artifact_content(
    State(state): State<SharedState>,
    _auth: Authed,
    Path(name): Path<String>,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    match state.store.get_current(&name) {
        Ok(Some((content, meta))) => build_content_response(content, &meta.content_type),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(e) => storage_error(e),
    }
}

/// (`create_only`, `update_only`) from the request headers; 400 when both are set.
fn create_update_flags(headers: &HeaderMap) -> Result<(bool, bool), Response> {
    let flag = |name: &str| {
        headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("true"))
    };
    let create_only = flag("x-create-only");
    let update_only = flag("x-update-only");
    if create_only && update_only {
        Err(error_response(
            StatusCode::BAD_REQUEST,
            "x-create-only and x-update-only are mutually exclusive",
        ))
    } else {
        Ok((create_only, update_only))
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
    let (create_only, update_only) = match create_update_flags(&headers) {
        Ok(flags) => flags,
        Err(resp) => return resp,
    };

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
        Ok(PutOutcome::Created) => {
            let _ = state.events.send(ArtifactEvent::Updated { name: name.clone() });
            (StatusCode::CREATED, Json(put_body(&state, &name, true))).into_response()
        }
        Ok(PutOutcome::Updated) => {
            let _ = state.events.send(ArtifactEvent::Updated { name: name.clone() });
            Json(put_body(&state, &name, false)).into_response()
        }
        Ok(PutOutcome::Unchanged) => Json(put_body(&state, &name, false)).into_response(),
        Err(e) => storage_error(e),
    }
}

fn put_body(state: &SharedState, name: &str, created: bool) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "url": public_url(state, "a", name),
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
        Ok(Some(meta)) => {
            let _ = state.events.send(ArtifactEvent::Updated { name: name.clone() });
            (StatusCode::OK, Json(meta)).into_response()
        }
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact or version not found"),
        Err(e) => storage_error(e),
    }
}

async fn rename_artifact(
    State(state): State<SharedState>,
    _auth: Authed,
    Path((name, new_name)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    if let Some(resp) = bad_name(&new_name) {
        return resp;
    }
    match state.store.rename(&name, &new_name) {
        Ok(RenameOutcome::Renamed(_)) => {
            let _ = state.events.send(ArtifactEvent::Renamed { name: name.clone(), to: new_name.clone() });
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "old_name": name,
                    "name": new_name,
                    "url": public_url(&state, "a", &new_name),
                })),
            )
                .into_response()
        }
        Ok(RenameOutcome::SourceMissing) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Ok(RenameOutcome::TargetExists) => error_response(StatusCode::CONFLICT, "target name already exists"),
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

// --- images ---

/// Images keep their own namespace and public path: `/i/{name}`.
async fn get_image_public(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    if !crate::store::valid_name(&name) {
        return not_found().await;
    }
    match state.images.get_current(&name) {
        Ok(Some((content, meta))) => build_content_response(content, &meta.content_type),
        Ok(None) => not_found().await,
        Err(e) => storage_error(e),
    }
}

async fn list_images(State(state): State<SharedState>, _auth: Authed, Query(params): Query<ListParams>) -> Response {
    match state.images.list(params.prefix.as_deref().unwrap_or("")) {
        Ok(items) => Json(items).into_response(),
        Err(e) => storage_error(e),
    }
}

async fn get_image_meta(State(state): State<SharedState>, _auth: Authed, Path(name): Path<String>) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    match state.images.get_meta(&name) {
        Ok(Some((meta, versions))) => {
            let body = serde_json::json!({
                "name": name,
                "url": public_url(&state, "i", &name),
                "meta": meta,
                "versions": versions,
            });
            Json(body).into_response()
        }
        Ok(None) => error_response(StatusCode::NOT_FOUND, "image not found"),
        Err(e) => storage_error(e),
    }
}

/// The stored content type is the request's, minus parameters — and it must be
/// an actual image type, so a stray HTML upload can't land in the namespace.
fn image_content_type(headers: &HeaderMap) -> Option<String> {
    let ct = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let bare = ct.split(';').next()?.trim().to_ascii_lowercase();
    bare.starts_with("image/").then_some(bare)
}

fn image_body(state: &SharedState, name: &str, created: bool, content_type: &str, body: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "url": public_url(state, "i", name),
        "content_type": content_type,
        "size": body.len(),
        "sha256": hex::encode(sha2::Sha256::digest(body)),
        "created": created,
    })
}

async fn put_image(
    State(state): State<SharedState>,
    _auth: Authed,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    let (create_only, update_only) = match create_update_flags(&headers) {
        Ok(flags) => flags,
        Err(resp) => return resp,
    };
    let Some(content_type) = image_content_type(&headers) else {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content type must be image/* (png, jpeg, gif, webp, avif, svg)",
        );
    };

    let exists = state.images.exists(&name);
    if create_only && exists {
        return error_response(StatusCode::CONFLICT, "image already exists");
    }
    if update_only && !exists {
        return error_response(StatusCode::NOT_FOUND, "image not found");
    }

    match state.images.put(&name, &body, &content_type) {
        Ok(PutOutcome::Created) => (
            StatusCode::CREATED,
            Json(image_body(&state, &name, true, &content_type, &body)),
        )
            .into_response(),
        Ok(PutOutcome::Updated | PutOutcome::Unchanged) => {
            Json(image_body(&state, &name, false, &content_type, &body)).into_response()
        }
        Err(e) => storage_error(e),
    }
}

async fn delete_image(State(state): State<SharedState>, _auth: Authed, Path(name): Path<String>) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    match state.images.delete(&name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "image not found"),
        Err(e) => storage_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use sha2::Digest;
    use tower::ServiceExt;

    fn test_state(dir: &std::path::Path) -> SharedState {
        Arc::new(AppState {
            store: FileStore::new(dir).unwrap(),
            images: FileStore::image_store(dir).unwrap(),
            token: b"secret-token".to_vec(),
            public_base: "https://artifacts.example.com".to_string(),
            events: tokio::sync::broadcast::channel(256).0,
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

    fn auth_put_image(
        name: &str,
        content_type: &str,
        body: Vec<u8>,
        extra: &[(&'static str, &'static str)],
    ) -> Request<Body> {
        let mut builder = Request::put(format!("/api/images/{name}"))
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, content_type);
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
            (axum::http::Method::GET, "/api/artifacts/x-report/content"),
            (axum::http::Method::PUT, "/api/artifacts/x-report"),
            (axum::http::Method::POST, "/api/artifacts/x-report/rename/x-new"),
            (axum::http::Method::DELETE, "/api/artifacts/x-report"),
            (axum::http::Method::GET, "/api/images"),
            (axum::http::Method::GET, "/api/images/x-shot"),
            (axum::http::Method::PUT, "/api/images/x-shot"),
            (axum::http::Method::DELETE, "/api/images/x-shot"),
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

        // public get serves content with stored content type; text/html
        // additionally carries the injected live script
        let (status, body, ct) = send(&app, Request::get("/a/x-report").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>two</h1>"));
        assert!(String::from_utf8_lossy(&body).contains("artifacts-live-scroll:"));

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
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>one</h1>"));

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
    async fn content_route_serves_raw_html_without_live_script() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(&app, auth_put("x-raw", "<html><body><p>hi</p></body></html>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);

        // authenticated content route: stored bytes verbatim, stored type
        let req = Request::get("/api/artifacts/x-raw/content")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, ct) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"<html><body><p>hi</p></body></html>");
        assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));

        // missing artifact is 404, invalid name is 400
        let req = Request::get("/api/artifacts/x-nope/content")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let req = Request::get("/api/artifacts/Bad_Name/content")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
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
    async fn image_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // public 404 before it exists
        let (status, _, _) = send(&app, Request::get("/i/x-shot").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // create with an image content type; the response carries the public URL
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let (status, body, _) = send(
            &app,
            auth_put_image("x-shot", "image/png", png.clone(), &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&body));
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["url"], "https://artifacts.example.com/i/x-shot");
        assert_eq!(created["content_type"], "image/png");
        assert_eq!(created["size"], png.len());
        assert_eq!(created["sha256"], hex::encode(sha2::Sha256::digest(&png)));
        assert_eq!(created["created"], true);

        // create-only again conflicts; update-only on a missing image is 404
        let (status, _, _) = send(
            &app,
            auth_put_image("x-shot", "image/png", png.clone(), &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _, _) = send(
            &app,
            auth_put_image("x-missing", "image/png", png.clone(), &[("x-update-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // non-image content types are rejected
        let (status, _, _) = send(&app, auth_put_image("x-bad", "text/html", b"<h1>hi</h1>".to_vec(), &[])).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // content type parameters are stripped: image/jpeg; charset=binary → image/jpeg
        let (status, _, _) = send(
            &app,
            auth_put_image("x-jpg", "image/jpeg; charset=binary", b"jpg".to_vec(), &[]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (_, _, ct) = send(&app, Request::get("/i/x-jpg").body(Body::empty()).unwrap()).await;
        assert_eq!(ct.as_deref(), Some("image/jpeg"));

        // update archives the previous bytes as a version
        let v2 = vec![0x89, b'P', b'N', b'G', 9, 9];
        let (status, _, _) = send(&app, auth_put_image("x-shot", "image/png", v2.clone(), &[])).await;
        assert_eq!(status, StatusCode::OK);

        // public serving returns the current bytes with the stored type
        let (status, body, ct) = send(&app, Request::get("/i/x-shot").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, v2);
        assert_eq!(ct.as_deref(), Some("image/png"));

        // meta shows one archived version and the public URL
        let req = Request::get("/api/images/x-shot")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["url"], "https://artifacts.example.com/i/x-shot");
        assert_eq!(meta["versions"].as_array().unwrap().len(), 1);
        assert_eq!(meta["versions"][0]["sha256"], hex::encode(sha2::Sha256::digest(&png)));

        // listing is image-only; the artifacts namespace stays empty
        let req = Request::get("/api/images?prefix=x-")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 2);
        let req = Request::get("/api/artifacts?prefix=x-")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 0);

        // delete, then public 404 again
        let req = Request::delete("/api/images/x-shot")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = send(&app, Request::get("/i/x-shot").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rename_moves_content_and_history() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(&app, auth_put("x-old", "<h1>one</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, _) = send(&app, auth_put("x-old", "<h1>two</h1>", &[])).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&app, auth_put("x-taken", "x", &[])).await;
        assert_eq!(status, StatusCode::CREATED);

        // target name already taken
        let (status, body, _) = send(
            &app,
            Request::post("/api/artifacts/x-old/rename/x-taken")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&body));

        // missing source
        let (status, _, _) = send(
            &app,
            Request::post("/api/artifacts/x-nope/rename/x-new")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // invalid target name
        let (status, _, _) = send(
            &app,
            Request::post("/api/artifacts/x-old/rename/Bad_Name")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // the rename itself
        let (status, body, _) = send(
            &app,
            Request::post("/api/artifacts/x-old/rename/x-new")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let renamed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(renamed["old_name"], "x-old");
        assert_eq!(renamed["name"], "x-new");
        assert_eq!(renamed["url"], "https://artifacts.example.com/a/x-new");

        // content and history live under the new name, the old URL redirects
        let (status, body, ct) = send(&app, Request::get("/a/x-new").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>two</h1>"));
        assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));
        let (status, _, _) = send(&app, Request::get("/a/x-old").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::FOUND);

        let req = Request::get("/api/artifacts/x-new")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let versions = meta["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0]["sha256"], hex::encode(sha2::Sha256::digest(b"<h1>one</h1>")));
    }

    #[tokio::test]
    async fn renamed_artifact_redirects_from_old_url() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(&app, auth_put("x-old", "<h1>hi</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, _) = send(
            &app,
            Request::post("/api/artifacts/x-old/rename/x-new")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // the old public URL answers with a redirect to the new one
        let resp = app
            .clone()
            .oneshot(Request::get("/a/x-old").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "https://artifacts.example.com/a/x-new"
        );

        // recreating the old name serves the artifact, no redirect
        let (status, _, _) = send(&app, auth_put("x-old", "<h1>fresh</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, body, _) = send(&app, Request::get("/a/x-old").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>fresh</h1>"));

        // a redirect to a deleted artifact is a plain 404
        let req = Request::delete("/api/artifacts/x-new")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = send(&app, Request::get("/a/x-new").body(Body::empty()).unwrap()).await;
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

    #[tokio::test]
    async fn html_serves_with_live_script_plain_is_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // spliced before the closing body tag, content untouched
        let (status, _, _) = send(&app, auth_put("x-html", "<html><body><p>hi</p></body></html>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let (_, body, _) = send(&app, Request::get("/a/x-html").body(Body::empty()).unwrap()).await;
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("EventSource"), "{html}");
        assert!(html.contains("<p>hi</p>"), "{html}");
        assert!(html.ends_with("</html>"), "{html}");

        // no closing tags: the script is appended after the content
        let (status, _, _) = send(&app, auth_put("x-bare", "<p>no shell</p>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let (_, body, _) = send(&app, Request::get("/a/x-bare").body(Body::empty()).unwrap()).await;
        let html = String::from_utf8_lossy(&body);
        assert!(html.starts_with("<p>no shell</p>"), "{html}");
        assert!(html.contains("EventSource"), "{html}");

        // non-HTML content is served byte-for-byte, no script
        let req = Request::put("/api/artifacts/x-plain")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("just text"))
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, body, _) = send(&app, Request::get("/a/x-plain").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"just text");
    }

    #[tokio::test]
    async fn mutations_publish_live_events() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let mut rx = state.events.subscribe();
        let app = router(state.clone(), 10 * 1024 * 1024);

        let (status, _, _) = send(&app, auth_put("x-live", "<h1>one</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(rx.try_recv().unwrap(), ArtifactEvent::Updated { name: "x-live".into() });

        let (status, _, _) = send(&app, auth_put("x-live", "<h1>two</h1>", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rx.try_recv().unwrap(), ArtifactEvent::Updated { name: "x-live".into() });

        // an identical write is silent
        let (status, _, _) = send(&app, auth_put("x-live", "<h1>two</h1>", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        let (status, _, _) = send(
            &app,
            Request::post("/api/artifacts/x-live/rename/x-moved")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            rx.try_recv().unwrap(),
            ArtifactEvent::Renamed { name: "x-live".into(), to: "x-moved".into() }
        );

        // rollback announces an update under the new name
        let req = Request::get("/api/artifacts/x-moved")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let version = meta["versions"][0]["version"].as_str().unwrap().to_string();
        let (status, _, _) = send(
            &app,
            Request::post(format!("/api/artifacts/x-moved/rollback/{version}"))
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rx.try_recv().unwrap(), ArtifactEvent::Updated { name: "x-moved".into() });
    }

    #[tokio::test]
    async fn live_stream_announces_the_new_name_on_connect() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(&app, auth_put("x-old", "<h1>hi</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, _) = send(
            &app,
            Request::post("/api/artifacts/x-old/rename/x-new")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // a stream opened on the old name learns the new name at once
        let resp = app
            .clone()
            .oneshot(Request::get("/a/x-old/live").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "text/event-stream");
        assert_eq!(resp.headers().get(header::CACHE_CONTROL).unwrap(), "no-cache");
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(frame.contains("event: renamed"), "{frame}");
        assert!(frame.contains(r#""to":"x-new""#), "{frame}");

        // a stream on a name with nothing behind it is a plain 404
        let (status, _, _) = send(&app, Request::get("/a/x-nope/live").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(&app, Request::get("/a/Bad_Name/live").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
