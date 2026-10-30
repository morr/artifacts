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
use subtle::ConstantTimeEq;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;

use crate::store::{FileStore, Meta, PutError, PutOutcome, RenameOutcome, NAME_RULE};

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
/// `project` is `None` for artifacts in the legacy flat namespace; streams
/// and events match on the (project, name) pair.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ArtifactEvent {
    Updated {
        project: Option<String>,
        name: String,
    },
    Renamed {
        project: Option<String>,
        name: String,
        to: String,
    },
}

impl ArtifactEvent {
    /// The (project, name) pair a stream must be watching to receive this event.
    fn about(&self) -> (&Option<String>, &str) {
        match self {
            ArtifactEvent::Updated { project, name }
            | ArtifactEvent::Renamed { project, name, .. } => (project, name),
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
        .route("/a/{project}/{name}", get(get_project_public))
        .route("/a/{name}/live", get(live_artifact))
        .route("/a/{project}/{name}/live", get(live_project_artifact))
        .route("/i/{name}", get(get_image_public))
        .route("/i/{project}/{name}", get(get_project_image_public))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/artifacts", get(list_artifacts))
        .route(
            "/api/artifacts/{name}",
            get(get_meta).put(put_artifact).delete(delete_artifact),
        )
        .route("/api/artifacts/{name}/content", get(get_artifact_content))
        .route("/api/artifacts/{name}/versions/{version}", get(get_version))
        .route("/api/artifacts/{name}/rollback/{version}", post(rollback))
        .route(
            "/api/artifacts/{name}/rename/{new_name}",
            post(rename_artifact),
        )
        .route("/api/images", get(list_images))
        .route(
            "/api/images/{name}",
            get(get_image_meta).put(put_image).delete(delete_image),
        )
        .route("/api/images/batch", post(batch_put_images))
        .fallback(not_found)
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .with_state(state)
}

// --- auth ---

struct Authed;

impl FromRequestParts<SharedState> for Authed {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
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
            Err(error_response(
                StatusCode::UNAUTHORIZED,
                "invalid or missing bearer token",
            ))
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
/// Project-scoped artifacts live at `<kind>/<project>/<name>`; the legacy
/// flat namespace stays at `<kind>/<name>`.
fn public_url(state: &SharedState, kind: &str, project: Option<&str>, name: &str) -> String {
    match project {
        Some(p) => format!(
            "{}/{}/{}/{}",
            state.public_base.trim_end_matches('/'),
            kind,
            p,
            name
        ),
        None => format!(
            "{}/{}/{}",
            state.public_base.trim_end_matches('/'),
            kind,
            name
        ),
    }
}

/// Canonical file extension for an image content type; unknown types get none.
fn image_ext(content_type: &str) -> Option<&'static str> {
    match content_type
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/avif" => Some("avif"),
        "image/svg+xml" => Some("svg"),
        _ => None,
    }
}

/// Public image URLs carry a file extension (`/i/<name>.png`) so links read as
/// images; unknown content types fall back to the bare name. `/i/<name>`
/// without an extension keeps serving for the URLs already published.
fn image_public_url(
    state: &SharedState,
    project: Option<&str>,
    name: &str,
    content_type: &str,
) -> String {
    match image_ext(content_type) {
        Some(ext) => format!("{}.{}", public_url(state, "i", project, name), ext),
        None => public_url(state, "i", project, name),
    }
}

// --- validation ---

fn bad_name(name: &str) -> Option<Response> {
    if crate::store::valid_name(name) {
        None
    } else {
        Some(error_response(StatusCode::BAD_REQUEST, NAME_RULE))
    }
}

/// Project namespaces use the same charset as names.
fn bad_project(project: Option<&str>) -> Option<Response> {
    match project {
        Some(p) if !crate::store::valid_name(p) => Some(error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid project: {NAME_RULE}"),
        )),
        _ => None,
    }
}

/// `X-Project: <slug>` — which namespace an API call addresses. Absent or
/// empty header means the legacy flat namespace.
fn project_from_headers(headers: &HeaderMap) -> Result<Option<String>, Response> {
    let Some(value) = headers.get("x-project") else {
        return Ok(None);
    };
    let raw = value.to_str().map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            "x-project must be a valid project slug",
        )
    })?;
    let project = raw.trim();
    let project = if project.is_empty() {
        None
    } else {
        Some(project.to_string())
    };
    if let Some(resp) = bad_project(project.as_deref()) {
        return Err(resp);
    }
    Ok(project)
}

/// Which namespace holds `name`: the project when it does, else the legacy
/// flat namespace (artifacts stored before projects existed), else nothing.
/// Reads, updates and deletes resolve with the fallback, so old artifacts
/// stay reachable from project sessions; creates never resolve — a create
/// lands in exactly the requested namespace.
fn resolve_namespace(
    exists: impl Fn(Option<&str>, &str) -> bool,
    project: Option<&str>,
    name: &str,
) -> Option<Option<String>> {
    if let Some(p) = project {
        if exists(Some(p), name) {
            return Some(Some(p.to_string()));
        }
    }
    if exists(None, name) {
        return Some(None);
    }
    None
}

/// The name form the injected live script uses in its URLs: `project/name`
/// for a project artifact, bare `name` for the legacy namespace.
fn logical_name(project: Option<&str>, name: &str) -> String {
    match project {
        Some(p) => format!("{p}/{name}"),
        None => name.to_string(),
    }
}

/// Version ids are fixed-width timestamps like 2026-10-09T18-22-33-123Z.
fn bad_version(version: &str) -> Option<Response> {
    let ok = version.len() == 24
        && version.ends_with('Z')
        && version[..23]
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'T');
    if ok {
        None
    } else {
        Some(error_response(
            StatusCode::BAD_REQUEST,
            "invalid version id",
        ))
    }
}

// --- public routes ---

async fn get_public(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    serve_public(&state, None, &name).await
}

async fn get_project_public(
    State(state): State<SharedState>,
    Path((project, name)): Path<(String, String)>,
) -> Response {
    serve_public(&state, Some(&project), &name).await
}

/// Serves `/a/<name>` and `/a/<project>/<name>`. Public URLs address their
/// namespace exactly — no legacy fallback here, unlike the API routes.
async fn serve_public(state: &SharedState, project: Option<&str>, name: &str) -> Response {
    if !crate::store::valid_name(name) || project.is_some_and(|p| !crate::store::valid_name(p)) {
        // Invalid names are indistinguishable from missing ones to the public.
        return not_found().await;
    }
    match state.store.get_current(project, name) {
        Ok(Some((content, meta))) => {
            let content = if meta.content_type.starts_with("text/html") {
                inject_live_script(content, &logical_name(project, name))
            } else {
                content
            };
            build_content_response(content, &meta.content_type)
        }
        Ok(None) => match state.store.resolve_alias(project, name) {
            // The artifact was renamed away: the old URL redirects while the
            // artifact still exists under the new name, same project.
            Ok(Some(target)) if state.store.exists(project, &target) => {
                redirect_response(state, project, &target)
            }
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
    serve_live(&state, None, &name).await
}

async fn live_project_artifact(
    State(state): State<SharedState>,
    Path((project, name)): Path<(String, String)>,
) -> Response {
    serve_live(&state, Some(&project), &name).await
}

async fn serve_live(state: &SharedState, project: Option<&str>, name: &str) -> Response {
    if !crate::store::valid_name(name) || project.is_some_and(|p| !crate::store::valid_name(p)) {
        return not_found().await;
    }
    let project = project.map(str::to_string);
    // Mirror the page route: a missing name with no live alias has nothing to
    // watch. A name renamed away keeps streaming — it announces the new name.
    let alias = match state.store.resolve_alias(project.as_deref(), name) {
        Ok(Some(to)) if state.store.exists(project.as_deref(), &to) => Some(to),
        Ok(_) => None,
        Err(e) => return storage_error(e),
    };
    if !state.store.exists(project.as_deref(), name) && alias.is_none() {
        return not_found().await;
    }
    let watched = project.clone();
    let watched_name = name.to_string();
    let updates =
        BroadcastStream::new(state.events.subscribe()).filter_map(move |item| match item {
            Ok(event) => {
                let (ev_project, ev_name) = event.about();
                (ev_project.as_ref() == watched.as_ref() && ev_name == watched_name)
                    .then(|| Ok(event.sse()))
            }
            // Fell behind and lost some events: a safe catch-up is a reload.
            Err(BroadcastStreamRecvError::Lagged(_)) => Some(Ok(ArtifactEvent::Updated {
                project: watched.clone(),
                name: watched_name.clone(),
            }
            .sse())),
        });
    let initial: Vec<Result<SseEvent, Infallible>> = alias
        .map(|to| {
            vec![Ok(ArtifactEvent::Renamed {
                project: project.clone(),
                name: name.to_string(),
                to,
            }
            .sse())]
        })
        .unwrap_or_default();
    let stream = tokio_stream::iter(initial).chain(updates);

    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
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
    var d = {};
    try { d = JSON.parse(ev.data); } catch (e) {}
    if (!d.to || d.to === name) return;
    // `name` is the logical path ("project/name" or bare "name"); the event's
    // `to` is the bare new name, so rebuild the path with its project
    var path = d.project ? d.project + "/" + d.to : d.to;
    if (path === name) return;
    save(path, "переименовано: " + path);
    location.replace("/a/" + path);
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
fn redirect_response(state: &SharedState, project: Option<&str>, target: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, public_url(state, "a", project, target))
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
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        &format!("storage error: {e}"),
    )
}

// --- api routes ---

#[derive(Deserialize)]
struct ListParams {
    prefix: Option<String>,
    /// Absent → everything; empty → legacy flat namespace only; a slug →
    /// that project only.
    project: Option<String>,
}

async fn list_artifacts(
    State(state): State<SharedState>,
    _auth: Authed,
    Query(params): Query<ListParams>,
) -> Response {
    match state.store.list(
        params.prefix.as_deref().unwrap_or(""),
        params.project.as_deref(),
    ) {
        Ok(items) => Json(items).into_response(),
        Err(e) => storage_error(e),
    }
}

async fn get_meta(
    State(state): State<SharedState>,
    _auth: Authed,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(ns) = resolve_namespace(|p, n| state.store.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };
    match state.store.get_meta(ns.as_deref(), &name) {
        Ok(Some((meta, versions))) => {
            let body = serde_json::json!({
                "name": name,
                "project": ns,
                "url": public_url(&state, "a", ns.as_deref(), &name),
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
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(ns) = resolve_namespace(|p, n| state.store.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };
    match state.store.get_current(ns.as_deref(), &name) {
        Ok(Some((content, meta))) => build_content_response(content, &meta.content_type),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(e) => storage_error(e),
    }
}

/// (`create_only`, `update_only`) from the request headers; 400 when both are set.
fn create_update_flags(headers: &HeaderMap) -> Result<(bool, bool), Response> {
    let flag = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
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

/// `X-Expected-Rev: <n>` — optimistic-concurrency guard for PUT: the write
/// applies only if `n` is the current rev, else 409 with the current one.
/// Absent header means unconditional.
fn expected_rev(headers: &HeaderMap) -> Result<Option<u64>, Response> {
    match headers.get("x-expected-rev") {
        None => Ok(None),
        Some(value) => match value
            .to_str()
            .map_err(|_| bad_expected_rev())?
            .trim()
            .parse::<u64>()
        {
            Ok(rev) => Ok(Some(rev)),
            Err(_) => Err(bad_expected_rev()),
        },
    }
}

fn bad_expected_rev() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "x-expected-rev must be a non-negative integer",
    )
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
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let expect_rev = match expected_rev(&headers) {
        Ok(rev) => rev,
        Err(resp) => return resp,
    };
    if create_only && expect_rev.is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "x-expected-rev guards updates and is meaningless with x-create-only",
        );
    }

    // Namespace resolution: a create lands exactly where asked (the same
    // name in another namespace is a different artifact and does not block
    // it); updates resolve the project first and fall back to the legacy
    // flat namespace; a put to a name that exists nowhere creates in the
    // requested namespace.
    let resolved = resolve_namespace(|p, n| state.store.exists(p, n), project.as_deref(), &name);
    if create_only {
        let taken = project
            .as_deref()
            .is_some_and(|p| state.store.exists(Some(p), &name))
            || (project.is_none() && state.store.exists(None, &name));
        if taken {
            return error_response(StatusCode::CONFLICT, "artifact already exists");
        }
    } else if update_only && resolved.is_none() {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    }
    let target = if create_only {
        project.clone()
    } else {
        // Some(None) means "resolved to the legacy namespace" — unwrap_or_else
        // keeps that None; flatten() would turn it into "unresolved" and fork
        // the update into the project namespace.
        resolved.unwrap_or_else(|| project.clone())
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/plain; charset=utf-8")
        .to_string();

    match state
        .store
        .put(target.as_deref(), &name, &body, &content_type, expect_rev)
    {
        Ok((PutOutcome::Created, meta)) => {
            let _ = state.events.send(ArtifactEvent::Updated {
                project: target.clone(),
                name: name.clone(),
            });
            (
                StatusCode::CREATED,
                Json(put_body(&state, target.as_deref(), &name, &meta, true)),
            )
                .into_response()
        }
        Ok((PutOutcome::Updated, meta)) => {
            let _ = state.events.send(ArtifactEvent::Updated {
                project: target.clone(),
                name: name.clone(),
            });
            Json(put_body(&state, target.as_deref(), &name, &meta, false)).into_response()
        }
        Ok((PutOutcome::Unchanged, meta)) => {
            Json(put_body(&state, target.as_deref(), &name, &meta, false)).into_response()
        }
        Err(PutError::RevConflict { current }) => {
            let expected = expect_rev
                .map(|r| r.to_string())
                .unwrap_or_else(|| "none".into());
            error_response(
                StatusCode::CONFLICT,
                &format!(
                    "version conflict: request expected rev {expected} but current is {current} — \
                     the artifact was updated by someone else; refetch it and re-apply your \
                     changes on top of the current content"
                ),
            )
        }
        Err(PutError::Io(e)) => storage_error(e),
    }
}

fn put_body(
    state: &SharedState,
    project: Option<&str>,
    name: &str,
    meta: &Meta,
    created: bool,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "project": project,
        "url": public_url(state, "a", project, name),
        "rev": meta.rev,
        "created": created,
    })
}

async fn get_version(
    State(state): State<SharedState>,
    _auth: Authed,
    Path((name, version)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    if let Some(resp) = bad_version(&version) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(ns) = resolve_namespace(|p, n| state.store.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "version not found");
    };
    match state.store.get_version(ns.as_deref(), &name, &version) {
        Ok(Some((content, info))) => build_content_response(content, &info.content_type),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "version not found"),
        Err(e) => storage_error(e),
    }
}

async fn rollback(
    State(state): State<SharedState>,
    _auth: Authed,
    Path((name, version)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    if let Some(resp) = bad_version(&version) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(ns) = resolve_namespace(|p, n| state.store.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "artifact or version not found");
    };
    match state.store.rollback(ns.as_deref(), &name, &version) {
        Ok(Some(meta)) => {
            let _ = state.events.send(ArtifactEvent::Updated {
                project: ns.clone(),
                name: name.clone(),
            });
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
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    if let Some(resp) = bad_name(&new_name) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // Renames stay inside the namespace the artifact lives in: resolving the
    // project first and falling back to the legacy one.
    let Some(ns) = resolve_namespace(|p, n| state.store.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };
    match state.store.rename(ns.as_deref(), &name, &new_name) {
        Ok(RenameOutcome::Renamed(_)) => {
            let _ = state.events.send(ArtifactEvent::Renamed {
                project: ns.clone(),
                name: name.clone(),
                to: new_name.clone(),
            });
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "old_name": name,
                    "name": new_name,
                    "project": ns,
                    "url": public_url(&state, "a", ns.as_deref(), &new_name),
                })),
            )
                .into_response()
        }
        Ok(RenameOutcome::SourceMissing) => {
            error_response(StatusCode::NOT_FOUND, "artifact not found")
        }
        Ok(RenameOutcome::TargetExists) => {
            error_response(StatusCode::CONFLICT, "target name already exists")
        }
        Err(e) => storage_error(e),
    }
}

async fn delete_artifact(
    State(state): State<SharedState>,
    _auth: Authed,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(ns) = resolve_namespace(|p, n| state.store.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };
    match state.store.delete(ns.as_deref(), &name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Err(e) => storage_error(e),
    }
}

// --- images ---

/// Images keep their own namespace and public path: `/i/{name}` and
/// `/i/{project}/{name}`.
async fn get_image_public(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    serve_image_public(&state, None, &name).await
}

async fn get_project_image_public(
    State(state): State<SharedState>,
    Path((project, name)): Path<(String, String)>,
) -> Response {
    serve_image_public(&state, Some(&project), &name).await
}

/// `/i/<name>.<ext>` and `/i/<name>` both serve the image; stored names never
/// contain dots, so the first dot starts a client-added extension. Exact
/// namespace, no legacy fallback — like the artifact page route.
async fn serve_image_public(state: &SharedState, project: Option<&str>, name: &str) -> Response {
    let name = name.split('.').next().unwrap_or("");
    if !crate::store::valid_name(name) || project.is_some_and(|p| !crate::store::valid_name(p)) {
        return not_found().await;
    }
    match state.images.get_current(project, name) {
        Ok(Some((content, meta))) => build_content_response(content, &meta.content_type),
        Ok(None) => not_found().await,
        Err(e) => storage_error(e),
    }
}

async fn list_images(
    State(state): State<SharedState>,
    _auth: Authed,
    Query(params): Query<ListParams>,
) -> Response {
    match state.images.list(
        params.prefix.as_deref().unwrap_or(""),
        params.project.as_deref(),
    ) {
        Ok(items) => Json(items).into_response(),
        Err(e) => storage_error(e),
    }
}

async fn get_image_meta(
    State(state): State<SharedState>,
    _auth: Authed,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(ns) = resolve_namespace(|p, n| state.images.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "image not found");
    };
    match state.images.get_meta(ns.as_deref(), &name) {
        Ok(Some((meta, versions))) => {
            let body = serde_json::json!({
                "name": name,
                "project": ns,
                "url": image_public_url(&state, ns.as_deref(), &name, &meta.content_type),
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

fn image_body(
    state: &SharedState,
    project: Option<&str>,
    name: &str,
    created: bool,
    meta: &Meta,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "project": project,
        "url": image_public_url(state, project, name, &meta.content_type),
        "content_type": meta.content_type,
        "size": meta.size,
        "sha256": meta.sha256,
        "rev": meta.rev,
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
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let expect_rev = match expected_rev(&headers) {
        Ok(rev) => rev,
        Err(resp) => return resp,
    };
    if create_only && expect_rev.is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "x-expected-rev guards updates and is meaningless with x-create-only",
        );
    }
    let Some(content_type) = image_content_type(&headers) else {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content type must be image/* (png, jpeg, gif, webp, avif, svg)",
        );
    };

    // Same namespace rules as artifacts: creates land exactly where asked,
    // updates resolve the project first and fall back to the legacy
    // namespace; a put to a name that exists nowhere creates in the
    // requested namespace.
    let resolved = resolve_namespace(|p, n| state.images.exists(p, n), project.as_deref(), &name);
    if create_only {
        let taken = project
            .as_deref()
            .is_some_and(|p| state.images.exists(Some(p), &name))
            || (project.is_none() && state.images.exists(None, &name));
        if taken {
            return error_response(StatusCode::CONFLICT, "image already exists");
        }
    } else if update_only && resolved.is_none() {
        return error_response(StatusCode::NOT_FOUND, "image not found");
    }
    let target = if create_only {
        project.clone()
    } else {
        // Some(None) means "resolved to the legacy namespace" — unwrap_or_else
        // keeps that None; flatten() would turn it into "unresolved" and fork
        // the update into the project namespace.
        resolved.unwrap_or_else(|| project.clone())
    };

    match state
        .images
        .put(target.as_deref(), &name, &body, &content_type, expect_rev)
    {
        Ok((PutOutcome::Created, meta)) => (
            StatusCode::CREATED,
            Json(image_body(&state, target.as_deref(), &name, true, &meta)),
        )
            .into_response(),
        Ok((PutOutcome::Updated | PutOutcome::Unchanged, meta)) => {
            Json(image_body(&state, target.as_deref(), &name, false, &meta)).into_response()
        }
        Err(PutError::RevConflict { current }) => {
            let expected = expect_rev
                .map(|r| r.to_string())
                .unwrap_or_else(|| "none".into());
            error_response(
                StatusCode::CONFLICT,
                &format!(
                    "version conflict: request expected rev {expected} but current is {current} — \
                     the image was updated by someone else; refetch it and re-apply your changes"
                ),
            )
        }
        Err(PutError::Io(e)) => storage_error(e),
    }
}

async fn delete_image(
    State(state): State<SharedState>,
    _auth: Authed,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = bad_name(&name) {
        return resp;
    }
    let project = match project_from_headers(&headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(ns) = resolve_namespace(|p, n| state.images.exists(p, n), project.as_deref(), &name)
    else {
        return error_response(StatusCode::NOT_FOUND, "image not found");
    };
    match state.images.delete(ns.as_deref(), &name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "image not found"),
        Err(e) => storage_error(e),
    }
}

// --- images: batch upload ---

const BATCH_MAX_ITEMS: usize = 50;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchImageItem {
    name: String,
    /// Raw image bytes, base64.
    content: String,
    /// Required — the API has no file extension or HTTP response to infer from.
    content_type: String,
    /// Overrides the batch-level project for this image.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Deserialize)]
struct BatchImages {
    /// false (default): create-only, an existing name is a per-item error;
    /// true: create or replace, the previous version is archived.
    #[serde(default)]
    replace: bool,
    /// Default project namespace for every item without its own `project`;
    /// absent means the legacy flat namespace.
    #[serde(default)]
    project: Option<String>,
    images: Vec<BatchImageItem>,
}

/// Batch image upload: one request, per-item results. Structural problems
/// (bad JSON, wrong shape, too many items) fail the whole request with 400;
/// anything specific to one image (invalid name, non-image content type,
/// existing name without `replace`) fails only that item — the answer is 200
/// and the other images still land. Results keep the request order, so a
/// client can retry exactly the failed rows.
async fn batch_put_images(
    State(state): State<SharedState>,
    _auth: Authed,
    body: axum::body::Bytes,
) -> Response {
    let batch: BatchImages = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("invalid batch body: {e}"))
        }
    };
    if batch.images.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "images must be a non-empty array");
    }
    if batch.images.len() > BATCH_MAX_ITEMS {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("at most {BATCH_MAX_ITEMS} images per batch"),
        );
    }

    let mut results = Vec::with_capacity(batch.images.len());
    for item in &batch.images {
        let project = item.project.clone().or_else(|| batch.project.clone());
        if let Some(p) = project.as_deref().filter(|p| !crate::store::valid_name(p)) {
            results.push(serde_json::json!({
                "name": item.name,
                "error": format!("invalid project \"{p}\": {NAME_RULE}"),
            }));
            continue;
        }
        match batch_item_outcome(&state, item, project.as_deref(), batch.replace) {
            Ok(row) => results.push(row),
            Err(message) => {
                results.push(serde_json::json!({ "name": item.name, "error": message }))
            }
        }
    }
    let uploaded = results
        .iter()
        .filter(|row| row.get("url").is_some())
        .count();
    let body = serde_json::json!({
        "uploaded": uploaded,
        "failed": results.len() - uploaded,
        "results": results,
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Decode, validate and store one batch item; the error string goes into the
/// per-item `error` field. Namespace rules match single-image puts: create
/// lands exactly in the requested project; replace resolves the project
/// first and falls back to the legacy namespace.
fn batch_item_outcome(
    state: &SharedState,
    item: &BatchImageItem,
    project: Option<&str>,
    replace: bool,
) -> Result<serde_json::Value, String> {
    use base64::Engine as _;
    if !crate::store::valid_name(&item.name) {
        return Err(format!("invalid name: {NAME_RULE}"));
    }
    let bare = item
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !bare.starts_with("image/") {
        return Err(format!(
            "contentType must be image/*, got \"{}\"",
            item.content_type
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&item.content)
        .map_err(|_| "content is empty or not valid base64".to_string())?;
    if bytes.is_empty() {
        return Err("content is empty or not valid base64".to_string());
    }
    let target = if replace {
        resolve_namespace(|p, n| state.images.exists(p, n), project, &item.name)
            .unwrap_or_else(|| project.map(str::to_string))
    } else {
        // create-only: exactly the requested namespace
        let taken = project.is_some_and(|p| state.images.exists(Some(p), &item.name))
            || (project.is_none() && state.images.exists(None, &item.name));
        if taken {
            return Err("image already exists (pass replace: true to overwrite)".to_string());
        }
        project.map(str::to_string)
    };
    match state
        .images
        .put(target.as_deref(), &item.name, &bytes, &bare, None)
    {
        Ok((outcome, meta)) => Ok(serde_json::json!({
            "name": item.name,
            "project": target,
            "url": image_public_url(state, target.as_deref(), &item.name, &bare),
            "content_type": meta.content_type,
            "size": meta.size,
            "sha256": meta.sha256,
            "created": matches!(outcome, PutOutcome::Created),
        })),
        Err(PutError::RevConflict { .. }) => {
            Err("unexpected rev conflict in batch put".to_string())
        }
        Err(PutError::Io(e)) => Err(format!("storage error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use base64::Engine as _;
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
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body, content_type)
    }

    fn auth_put(
        name: &str,
        body: &'static str,
        extra: &[(&'static str, &'static str)],
    ) -> Request<Body> {
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
            (
                axum::http::Method::POST,
                "/api/artifacts/x-report/rename/x-new",
            ),
            (axum::http::Method::DELETE, "/api/artifacts/x-report"),
            (axum::http::Method::GET, "/api/images"),
            (axum::http::Method::GET, "/api/images/x-shot"),
            (axum::http::Method::PUT, "/api/images/x-shot"),
            (axum::http::Method::DELETE, "/api/images/x-shot"),
            (axum::http::Method::POST, "/api/images/batch"),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap();
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
        let (status, _, _) = send(
            &app,
            Request::get("/a/x-report").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // create-only
        let (status, body, _) = send(
            &app,
            auth_put("x-report", "<h1>one</h1>", &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );

        // second create-only conflicts
        let (status, _, _) = send(
            &app,
            auth_put("x-report", "<h1>two</h1>", &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // update
        let (status, _, _) = send(&app, auth_put("x-report", "<h1>two</h1>", &[])).await;
        assert_eq!(status, StatusCode::OK);

        // public get serves content with stored content type; text/html
        // additionally carries the injected live script
        let (status, body, ct) = send(
            &app,
            Request::get("/a/x-report").body(Body::empty()).unwrap(),
        )
        .await;
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
        let (_, body, _) = send(
            &app,
            Request::get("/a/x-report").body(Body::empty()).unwrap(),
        )
        .await;
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>one</h1>"));

        // delete, then public 404 again
        let req = Request::delete("/api/artifacts/x-report")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = send(
            &app,
            Request::get("/a/x-report").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn content_route_serves_raw_html_without_live_script() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(
            &app,
            auth_put("x-raw", "<html><body><p>hi</p></body></html>", &[]),
        )
        .await;
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
        let (status, _, _) = send(
            &app,
            Request::get("/a/Bad_Name").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // unknown paths never leak that this is an artifact store
        let (status, _, _) = send(&app, Request::get("/").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(&app, Request::get("/api").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn conditional_update_is_guarded_by_expected_rev() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // create reports rev 1
        let (status, body, _) = send(&app, auth_put("x-rev", "<h1>one</h1>", &[])).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["rev"], 1);

        // matching rev updates and reports the bumped one
        let (status, body, _) = send(
            &app,
            auth_put("x-rev", "<h1>two</h1>", &[("x-expected-rev", "1")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let updated: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(updated["rev"], 2);

        // stale rev is a 409 naming the current one; the content stays put
        let (status, body, _) = send(
            &app,
            auth_put("x-rev", "<h1>three</h1>", &[("x-expected-rev", "1")]),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains("current is 2"));
        let (_, body, _) = send(&app, Request::get("/a/x-rev").body(Body::empty()).unwrap()).await;
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>two</h1>"));

        // matching rev with identical content: Unchanged, rev stays
        let (status, body, _) = send(
            &app,
            auth_put("x-rev", "<h1>two</h1>", &[("x-expected-rev", "2")]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let unchanged: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(unchanged["rev"], 2);

        // the meta route carries the rev too
        let req = Request::get("/api/artifacts/x-rev")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (_, body, _) = send(&app, req).await;
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["meta"]["rev"], 2);

        // x-expected-rev with x-create-only, and a garbage header, are 400
        let (status, _, _) = send(
            &app,
            auth_put(
                "x-other",
                "x",
                &[("x-create-only", "true"), ("x-expected-rev", "1")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = send(&app, auth_put("x-rev", "x", &[("x-expected-rev", "abc")])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // conditional update of a missing artifact stays the regular 404
        let (status, _, _) = send(
            &app,
            auth_put(
                "x-missing",
                "x",
                &[("x-update-only", "true"), ("x-expected-rev", "1")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn image_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // public 404 before it exists
        let (status, _, _) =
            send(&app, Request::get("/i/x-shot").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // create with an image content type; the response carries the public URL
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let (status, body, _) = send(
            &app,
            auth_put_image(
                "x-shot",
                "image/png",
                png.clone(),
                &[("x-create-only", "true")],
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["url"], "https://artifacts.example.com/i/x-shot.png");
        assert_eq!(created["content_type"], "image/png");
        assert_eq!(created["size"], png.len());
        assert_eq!(created["sha256"], hex::encode(sha2::Sha256::digest(&png)));
        assert_eq!(created["created"], true);

        // create-only again conflicts; update-only on a missing image is 404
        let (status, _, _) = send(
            &app,
            auth_put_image(
                "x-shot",
                "image/png",
                png.clone(),
                &[("x-create-only", "true")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _, _) = send(
            &app,
            auth_put_image(
                "x-missing",
                "image/png",
                png.clone(),
                &[("x-update-only", "true")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // non-image content types are rejected
        let (status, _, _) = send(
            &app,
            auth_put_image("x-bad", "text/html", b"<h1>hi</h1>".to_vec(), &[]),
        )
        .await;
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
        let (status, _, _) =
            send(&app, auth_put_image("x-shot", "image/png", v2.clone(), &[])).await;
        assert_eq!(status, StatusCode::OK);

        // public serving returns the current bytes with the stored type
        let (status, body, ct) =
            send(&app, Request::get("/i/x-shot").body(Body::empty()).unwrap()).await;
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
        assert_eq!(meta["url"], "https://artifacts.example.com/i/x-shot.png");
        assert_eq!(meta["versions"].as_array().unwrap().len(), 1);
        assert_eq!(
            meta["versions"][0]["sha256"],
            hex::encode(sha2::Sha256::digest(&png))
        );

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
        let (status, _, _) =
            send(&app, Request::get("/i/x-shot").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn image_url_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // the public URL gains the canonical extension of the content type
        // (parameters stripped); a type without a canonical one keeps the bare name
        let cases = [
            (
                "x-e-png",
                "image/png",
                "https://artifacts.example.com/i/x-e-png.png",
            ),
            (
                "x-e-jpg",
                "image/jpeg; charset=binary",
                "https://artifacts.example.com/i/x-e-jpg.jpg",
            ),
            (
                "x-e-svg",
                "image/svg+xml",
                "https://artifacts.example.com/i/x-e-svg.svg",
            ),
            (
                "x-e-tiff",
                "image/tiff",
                "https://artifacts.example.com/i/x-e-tiff",
            ),
        ];
        for (name, ct, url) in cases {
            let (status, body, _) = send(
                &app,
                auth_put_image(name, ct, b"img".to_vec(), &[("x-create-only", "true")]),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::CREATED,
                "{}",
                String::from_utf8_lossy(&body)
            );
            let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(created["url"], url);

            // the extensioned path serves the same bytes as the bare one
            let path: axum::http::Uri = url
                .trim_start_matches("https://artifacts.example.com")
                .parse()
                .unwrap();
            let bare: axum::http::Uri = format!("/i/{name}").parse().unwrap();
            for get in [path, bare] {
                let (status, _, served) =
                    send(&app, Request::get(get).body(Body::empty()).unwrap()).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(served.as_deref(), Some(ct.split(';').next().unwrap()));
            }
        }

        // the extension is decoration: even a wrong one still serves the image,
        // while an all-extension path is just a missing name
        let (status, _, _) = send(
            &app,
            Request::get("/i/x-e-png.webp").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&app, Request::get("/i/.png").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn batch_image_upload() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);

        // happy path: two images in one request, results keep the request order
        let batch = serde_json::json!({
            "images": [
                { "name": "x-b-one", "content": encoded, "contentType": "image/png" },
                { "name": "x-b-two", "content": base64::engine::general_purpose::STANDARD.encode(b"gif"), "contentType": "image/gif" },
            ],
        });
        let req = Request::post("/api/images/batch")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&batch).unwrap()))
            .unwrap();
        let (status, body, ct) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("application/json"));
        let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(batch["uploaded"], 2);
        assert_eq!(batch["failed"], 0);
        assert_eq!(
            batch["results"][0]["url"],
            "https://artifacts.example.com/i/x-b-one.png"
        );
        assert_eq!(batch["results"][0]["created"], true);
        assert_eq!(batch["results"][0]["size"], png.len());
        assert_eq!(
            batch["results"][0]["sha256"],
            hex::encode(sha2::Sha256::digest(&png))
        );
        assert_eq!(
            batch["results"][1]["url"],
            "https://artifacts.example.com/i/x-b-two.gif"
        );
        let (status, body, _) = send(
            &app,
            Request::get("/i/x-b-two.gif").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"gif");

        // per-item failures don't sink the rest: duplicate name, invalid name,
        // non-image type, broken base64
        let batch = serde_json::json!({
            "images": [
                { "name": "x-b-one", "content": encoded, "contentType": "image/png" },
                { "name": "Bad_Name", "content": encoded, "contentType": "image/png" },
                { "name": "x-b-html", "content": encoded, "contentType": "text/html" },
                { "name": "x-b-b64", "content": "!!!not base64!!!", "contentType": "image/png" },
                { "name": "x-b-three", "content": encoded, "contentType": "image/png" },
            ],
        });
        let req = Request::post("/api/images/batch")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::from(serde_json::to_vec(&batch).unwrap()))
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(batch["uploaded"], 1);
        assert_eq!(batch["failed"], 4);
        let results = batch["results"].as_array().unwrap();
        assert!(results[0]["error"]
            .as_str()
            .unwrap()
            .contains("already exists"));
        assert!(results[1]["error"]
            .as_str()
            .unwrap()
            .contains("invalid name"));
        assert!(results[2]["error"].as_str().unwrap().contains("image/*"));
        assert!(results[3]["error"].as_str().unwrap().contains("base64"));
        assert_eq!(
            results[4]["url"],
            "https://artifacts.example.com/i/x-b-three.png"
        );

        // replace: true overwrites the duplicate instead of erroring
        let batch = serde_json::json!({
            "replace": true,
            "images": [
                { "name": "x-b-one", "content": base64::engine::general_purpose::STANDARD.encode(b"v2"), "contentType": "image/png" },
            ],
        });
        let req = Request::post("/api/images/batch")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::from(serde_json::to_vec(&batch).unwrap()))
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(batch["uploaded"], 1);
        assert_eq!(batch["results"][0]["created"], false);
        let (_, body, _) = send(
            &app,
            Request::get("/i/x-b-one").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(body, b"v2");

        // structural problems fail the whole request with 400
        for broken in [
            serde_json::json!({ "images": [] }),
            serde_json::json!({ "images": "two" }),
            serde_json::json!({ "replace": false }),
            serde_json::json!(null),
        ] {
            let req = Request::post("/api/images/batch")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .body(Body::from(serde_json::to_vec(&broken).unwrap()))
                .unwrap();
            let (status, _, _) = send(&app, req).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{broken}");
        }

        // more items than the cap is 400 too
        let many: Vec<serde_json::Value> = (0..51)
            .map(|i| serde_json::json!({ "name": format!("x-b-{i}"), "content": encoded, "contentType": "image/png" }))
            .collect();
        let req = Request::post("/api/images/batch")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({ "images": many })).unwrap(),
            ))
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
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
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{}",
            String::from_utf8_lossy(&body)
        );

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
        let (status, body, ct) =
            send(&app, Request::get("/a/x-new").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>two</h1>"));
        assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));
        let (status, _, _) =
            send(&app, Request::get("/a/x-old").body(Body::empty()).unwrap()).await;
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
        assert_eq!(
            versions[0]["sha256"],
            hex::encode(sha2::Sha256::digest(b"<h1>one</h1>"))
        );
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
        let (status, body, _) =
            send(&app, Request::get("/a/x-old").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>fresh</h1>"));

        // a redirect to a deleted artifact is a plain 404
        let req = Request::delete("/api/artifacts/x-new")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) =
            send(&app, Request::get("/a/x-new").body(Body::empty()).unwrap()).await;
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
        let (status, _, _) = send(
            &app,
            auth_put("x-html", "<html><body><p>hi</p></body></html>", &[]),
        )
        .await;
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
        let (status, body, _) = send(
            &app,
            Request::get("/a/x-plain").body(Body::empty()).unwrap(),
        )
        .await;
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
        assert_eq!(
            rx.try_recv().unwrap(),
            ArtifactEvent::Updated {
                project: None,
                name: "x-live".into()
            }
        );

        let (status, _, _) = send(&app, auth_put("x-live", "<h1>two</h1>", &[])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            rx.try_recv().unwrap(),
            ArtifactEvent::Updated {
                project: None,
                name: "x-live".into()
            }
        );

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
            ArtifactEvent::Renamed {
                project: None,
                name: "x-live".into(),
                to: "x-moved".into()
            }
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
        assert_eq!(
            rx.try_recv().unwrap(),
            ArtifactEvent::Updated {
                project: None,
                name: "x-moved".into()
            }
        );
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
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
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
        let (status, _, _) = send(
            &app,
            Request::get("/a/x-nope/live").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(
            &app,
            Request::get("/a/Bad_Name/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn auth_put_project(
        project: &'static str,
        name: &str,
        body: &'static str,
        extra: &[(&'static str, &'static str)],
    ) -> Request<Body> {
        let mut all: Vec<(&'static str, &'static str)> = vec![("x-project", project)];
        all.extend_from_slice(extra);
        auth_put(name, body, &all)
    }

    #[tokio::test]
    async fn project_artifacts_are_scoped_by_x_project() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // create in a project: the response names the namespace and URL
        let (status, body, _) = send(
            &app,
            auth_put_project(
                "minisklad",
                "x-page",
                "<h1>shop</h1>",
                &[("x-create-only", "true")],
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["project"], "minisklad");
        assert_eq!(
            created["url"],
            "https://artifacts.example.com/a/minisklad/x-page"
        );

        // the same name in the legacy namespace and another project are
        // independent artifacts, and the project create did not touch them
        let (status, body, _) = send(
            &app,
            auth_put_project("kladovkin", "x-page", "<h1>other</h1>", &[]),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let (status, _, _) = send(
            &app,
            auth_put("x-page", "<h1>legacy</h1>", &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // public routes address namespaces exactly
        let (status, body, _) = send(
            &app,
            Request::get("/a/minisklad/x-page")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>shop</h1>"));
        let (status, body, _) = send(
            &app,
            Request::get("/a/kladovkin/x-page")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>other</h1>"));
        let (status, body, _) =
            send(&app, Request::get("/a/x-page").body(Body::empty()).unwrap()).await;
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>legacy</h1>"));

        // reads resolve the project first: with X-Project the project copy
        // wins; without it the legacy one
        let req = Request::get("/api/artifacts/x-page/content")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header("x-project", "minisklad")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"<h1>shop</h1>");
        let req = Request::get("/api/artifacts/x-page")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (_, body, _) = send(&app, req).await;
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["project"], serde_json::Value::Null);
        assert_eq!(meta["url"], "https://artifacts.example.com/a/x-page");

        // a name that exists only in the legacy namespace is still readable
        // with X-Project set — the fallback keeps old artifacts reachable
        let (status, _, _) = send(&app, auth_put("x-old", "legacy-only", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let req = Request::get("/api/artifacts/x-old/content")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header("x-project", "minisklad")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"legacy-only");

        // a name that exists nowhere is 404 even with X-Project
        let req = Request::get("/api/artifacts/x-missing")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header("x-project", "minisklad")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // an invalid project slug is a 400
        let (status, _, _) = send(
            &app,
            auth_put_project(
                "Bad_Slug",
                "x-page",
                "<h1>nope</h1>",
                &[("x-create-only", "true")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn legacy_artifact_update_from_project_session_stays_in_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        // the artifact predates projects and lives in the legacy namespace
        let (status, body, _) = send(&app, auth_put("x-plan", "<h1>v1</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["rev"], 1);

        // a project session updates it guarded by the legacy rev: the update
        // must land in the legacy namespace — not 409 against an empty project
        // namespace and not fork a copy into it
        let (status, body, _) = send(
            &app,
            auth_put_project(
                "minisklad",
                "x-plan",
                "<h1>v2</h1>",
                &[("x-update-only", "true"), ("x-expected-rev", "1")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let updated: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(updated["project"], serde_json::Value::Null);
        assert_eq!(updated["rev"], 2);
        assert_eq!(updated["created"], false);

        // a session read still falls back to the one updated artifact
        let req = Request::get("/api/artifacts/x-plan")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header("x-project", "minisklad")
            .body(Body::empty())
            .unwrap();
        let (_, body, _) = send(&app, req).await;
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["project"], serde_json::Value::Null);
        assert_eq!(meta["meta"]["rev"], 2);

        // the project namespace gained no copy
        let req = Request::get("/api/artifacts?project=minisklad")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (_, body, _) = send(&app, req).await;
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn legacy_image_update_from_project_session_stays_in_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(
            &app,
            auth_put_image("x-shot", "image/png", b"v1".to_vec(), &[]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // the guarded single-image update resolves to the legacy namespace
        let (status, body, _) = send(
            &app,
            auth_put_image(
                "x-shot",
                "image/png",
                b"v2".to_vec(),
                &[
                    ("x-project", "minisklad"),
                    ("x-update-only", "true"),
                    ("x-expected-rev", "1"),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let updated: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(updated["project"], serde_json::Value::Null);
        assert_eq!(updated["rev"], 2);

        // batch replace resolves the same way
        let batch = serde_json::json!({
            "replace": true,
            "project": "minisklad",
            "images": [
                { "name": "x-shot", "content": base64::engine::general_purpose::STANDARD.encode(b"v3"), "contentType": "image/png" },
            ],
        });
        let req = Request::post("/api/images/batch")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&batch).unwrap()))
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(batch["uploaded"], 1);
        assert_eq!(batch["results"][0]["project"], serde_json::Value::Null);

        // the legacy image carries the batch content; the project namespace
        // gained no copy
        let (status, body, _) = send(
            &app,
            Request::get("/i/x-shot").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"v3");
        let req = Request::get("/api/images?project=minisklad")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (_, body, _) = send(&app, req).await;
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn project_lists_filter_and_tag_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(
            &app,
            auth_put("x-legacy", "0", &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, _) = send(
            &app,
            auth_put_project("alpha", "x-item", "1", &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, _) = send(
            &app,
            auth_put_project("beta", "x-item", "2", &[("x-create-only", "true")]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // everything: legacy first, then per project
        let req = Request::get("/api/artifacts")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["project"], serde_json::Value::Null);
        assert_eq!(items[0]["name"], "x-legacy");
        assert_eq!(items[1]["project"], "alpha");
        assert_eq!(items[2]["project"], "beta");

        // one project only
        let req = Request::get("/api/artifacts?project=alpha")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["project"], "alpha");

        // the empty project value lists only the legacy namespace
        let req = Request::get("/api/artifacts?project=")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["project"], serde_json::Value::Null);

        // images list with the same filter
        let req = Request::put("/api/images/x-shot")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "image/png")
            .header("x-project", "alpha")
            .body(Body::from("png"))
            .unwrap();
        let (status, _, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::CREATED);
        let req = Request::get("/api/images?project=alpha")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 1);
        assert_eq!(items[0]["project"], "alpha");
        let req = Request::get("/api/images")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn project_rename_scopes_redirect_and_url() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);

        let (status, _, _) = send(
            &app,
            auth_put_project("minisklad", "x-old", "<h1>hi</h1>", &[]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        // a legacy artifact with the same name: its alias must stay untouched
        let (status, _, _) = send(&app, auth_put("x-old", "<h1>legacy</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, body, _) = send(
            &app,
            Request::post("/api/artifacts/x-old/rename/x-new")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .header("x-project", "minisklad")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let renamed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(renamed["project"], "minisklad");
        assert_eq!(
            renamed["url"],
            "https://artifacts.example.com/a/minisklad/x-new"
        );

        // the project URL redirects within the project...
        let resp = app
            .clone()
            .oneshot(
                Request::get("/a/minisklad/x-old")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "https://artifacts.example.com/a/minisklad/x-new"
        );
        // ...the legacy name never redirects — it is a different artifact
        let (status, body, _) =
            send(&app, Request::get("/a/x-old").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).starts_with("<h1>legacy</h1>"));
    }

    #[tokio::test]
    async fn project_images_public_route_and_batch() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_state(dir.path()), 10 * 1024 * 1024);
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"png1");

        // batch upload into a project; per-item project overrides the default
        let batch = serde_json::json!({
            "project": "minisklad",
            "images": [
                { "name": "x-logo", "content": encoded, "contentType": "image/png" },
                { "name": "x-other", "content": encoded, "contentType": "image/png", "project": "kladovkin" },
            ],
        });
        let req = Request::post("/api/images/batch")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&batch).unwrap()))
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(batch["uploaded"], 2);
        assert_eq!(batch["results"][0]["project"], "minisklad");
        assert_eq!(
            batch["results"][0]["url"],
            "https://artifacts.example.com/i/minisklad/x-logo.png"
        );
        assert_eq!(batch["results"][1]["project"], "kladovkin");

        // the extensioned project URL serves the bytes; the bare project URL too
        let (status, _, _) = send(
            &app,
            Request::get("/i/minisklad/x-logo.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body, _) = send(
            &app,
            Request::get("/i/minisklad/x-logo")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"png1");
        // ...another project's namespace does not have this name
        let (status, _, _) = send(
            &app,
            Request::get("/i/kladovkin/x-logo.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // single-image create lands in the project
        let req = Request::put("/api/images/x-shot")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "image/png")
            .header("x-project", "minisklad")
            .body(Body::from("shot"))
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::CREATED);
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["project"], "minisklad");
        assert_eq!(
            created["url"],
            "https://artifacts.example.com/i/minisklad/x-shot.png"
        );

        // an unconditional put to a name that exists nowhere creates in the
        // requested project, it never falls back to another namespace
        let req = Request::put("/api/images/x-shot")
            .header(header::AUTHORIZATION, "Bearer secret-token")
            .header(header::CONTENT_TYPE, "image/png")
            .header("x-project", "kladovkin")
            .body(Body::from("theirs"))
            .unwrap();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::CREATED);
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["project"], "kladovkin");
    }

    #[tokio::test]
    async fn project_live_stream_and_events_carry_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let mut rx = state.events.subscribe();
        let app = router(state.clone(), 10 * 1024 * 1024);

        let (status, _, _) = send(
            &app,
            auth_put_project("minisklad", "x-live", "<h1>one</h1>", &[]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            rx.try_recv().unwrap(),
            ArtifactEvent::Updated {
                project: Some("minisklad".into()),
                name: "x-live".into()
            }
        );

        // events of other namespaces do not wake a project stream
        let (status, _, _) = send(&app, auth_put("x-live", "<h1>legacy</h1>", &[])).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(matches!(
            rx.try_recv(),
            Ok(ArtifactEvent::Updated { project: None, name }) if name == "x-live"
        ));

        // the project page injects the logical path, so the live stream and a
        // rename follow project URLs
        let (status, body, _) = send(
            &app,
            Request::get("/a/minisklad/x-live")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains(r#"var name = "minisklad/x-live""#));

        let (status, _, _) = send(
            &app,
            Request::post("/api/artifacts/x-live/rename/x-moved")
                .header(header::AUTHORIZATION, "Bearer secret-token")
                .header("x-project", "minisklad")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            rx.try_recv().unwrap(),
            ArtifactEvent::Renamed {
                project: Some("minisklad".into()),
                name: "x-live".into(),
                to: "x-moved".into()
            }
        );

        let resp = app
            .clone()
            .oneshot(
                Request::get("/a/minisklad/x-live/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(frame.contains(r#""to":"x-moved""#), "{frame}");
        assert!(frame.contains(r#""project":"minisklad""#), "{frame}");
    }
}
