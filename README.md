# artifacts

Tiny artifact storage: a Rust web server (single static musl binary) that stores
files on its own filesystem, plus an MCP server for AI agents to work with them.

- **Public** (no auth): `GET /a/<name>` — serves the current version of an
  artifact with its stored content type (HTML renders in a browser); `GET /i/<name>.<ext>` —
  serves an uploaded image the same way (the extension is decoration: the stored
  name never contains dots, so whatever follows the first dot is stripped; the
  bare `/i/<name>` keeps serving for links already published); `GET /healthz`.
  A renamed artifact keeps answering on its old URL with a 302 redirect to the
  new name, as long as nothing was created under the old name since.
- **Live reload**: served HTML pages carry a tiny injected script holding a
  Server-Sent Events connection to `GET /a/<name>/live`. When the artifact is
  updated (put or rollback), the open page reloads itself keeping the scroll
  position; when it is renamed, the page follows to the new URL. Both come with
  a small fading toast, so a reader knows why the page changed. The stream is
  404 for names that have neither an artifact nor a live alias.
- **Everything else** requires `Authorization: Bearer $ARTIFACTS_TOKEN`:
  list, metadata, versions, upload/update, rollback, rename, delete — for both
  artifacts and images. There is no public
  listing and never will be — artifact names are only visible with the token.
- Updates archive the previous version (`versions/<timestamp>`), so anything can
  be rolled back.
- **Optimistic concurrency**: every artifact and image carries a `rev` — 1 on
  create, +1 on every content-changing update and rollback. A PUT with
  `X-Expected-Rev: <n>` applies only when `n` is the current rev and answers 409
  with the current one otherwise; the check and the write are serialized under a
  single write lock, so a guarded write can never interleave with another
  mutation. Parallel agents pass the rev they read and, on a conflict, refetch
  the artifact and re-apply their changes on top.

Artifact and image names must match `^[a-z0-9][a-z0-9-]{0,63}$` — one safe path component.
Artifacts and images are two independent namespaces: the same name can exist in both,
and deleting one never touches the other. Images are meant to be embedded into
artifacts by their public URL (`<img src="/i/<name>">`); there is no rename or
rollback for images — replace with an update or delete and re-upload.

## HTTP API

| Method | Path | Notes |
|---|---|---|
| GET | `/a/{name}` | public; current content |
| GET | `/a/{name}/live` | public; SSE stream of `updated` / `renamed` events for the artifact |
| GET | `/i/{name}` | public; current image bytes |
| GET | `/healthz` | public; for kamal-proxy |
| GET | `/api/artifacts?prefix=` | list: name, content type, size, updated_at |
| GET | `/api/artifacts/{name}` | meta (incl. `rev`) + version history + public URL |
| GET | `/api/artifacts/{name}/content` | raw current bytes with the stored content type — unlike the public `GET /a/{name}`, text/html is served without the injected live-reload script |
| PUT | `/api/artifacts/{name}` | body = content; `X-Create-Only: true` → 409 if exists; `X-Update-Only: true` → 404 if missing; otherwise upsert. `X-Expected-Rev: <n>` → the write applies only if `n` is the current rev, else 409 naming the current one. Every response carries the stored `rev`. Content type taken from the request `Content-Type` |
| GET | `/api/artifacts/{name}/versions/{version}` | download a specific version |
| POST | `/api/artifacts/{name}/rollback/{version}` | make a version current (outgoing current is archived) |
| POST | `/api/artifacts/{name}/rename/{new}` | atomic move; 409 if the target name exists; content, history and timestamps are preserved; the old URL answers with a 302 redirect (aliases follow further renames; recreating a name drops its redirect) |
| DELETE | `/api/artifacts/{name}` | remove the artifact with all versions |
| GET | `/api/images?prefix=` | list uploaded images |
| GET | `/api/images/{name}` | meta (incl. `rev`) + version history + public image URL |
| PUT | `/api/images/{name}` | body = image bytes; request `Content-Type` must be `image/*` (parameters are stripped), else 415; same `X-Create-Only` / `X-Update-Only` / `X-Expected-Rev` semantics as artifacts. Response carries the public URL (`/i/<name>.<ext>`, the canonical extension of the content type — png, jpg, svg, …; unknown types keep the bare name), content type, size, sha256 and rev |
| POST | `/api/images/batch` | body = JSON `{"replace": false, "images": [{"name", "content" (base64), "contentType"}]}`; up to 50 images and one `ARTIFACTS_MAX_BODY` worth of bytes per call. Structural problems (bad JSON, empty or oversized array) are 400; anything specific to one image — invalid name, non-image `contentType`, existing name without `replace` — fails only that item, and the structurally valid answer is 200 with `{"uploaded", "failed", "results"}` in the request order (each row either the usual image body or `{"name", "error"}`) |
| DELETE | `/api/images/{name}` | remove the image with all versions |

## Configuration (env)

| Var | Default | Meaning |
|---|---|---|
| `ARTIFACTS_TOKEN` | — (required) | bearer token for `/api/*` |
| `PORT` | `8080` | listen port |
| `ARTIFACTS_DATA_DIR` | `./data` | storage root (`/data` in the image) |
| `ARTIFACTS_MAX_BODY` | `10485760` | request body limit, bytes |
| `ARTIFACTS_PUBLIC_BASE` | `http://localhost:$PORT` | base for URLs returned by the API |

## Layout on disk

```
<data>/artifacts/<name>/
  current              # current version content
  meta.json            # rev, content_type, created_at, updated_at, size, sha256
  versions/2026-10-09T18-22-33-123Z       # archived content
  versions/2026-10-09T18-22-33-123Z.json  # its meta at save time
<data>/images/<name>/  # same structure, image namespace (no rename, no aliases)
<data>/aliases/<old-name>  # renamed artifact: file content is the new name
```

All writes are atomic (tmp + rename).

## MCP server (`mcp/`)

TypeScript MCP server over stdio with tools `list_artifacts`, `get_artifact`,
`save_artifact` (create-only), `update_artifact` (update-only), `list_versions`,
`rollback_artifact`, `rename_artifact`, `delete_artifact`, plus the image tools
`upload_image` / `update_image` (sources: local `filePath`, `sourceUrl` or
`contentBase64`), the batch `upload_images` (same per-item sources, per-item
results — the alternative to N single calls when several images go into one
page), `get_image` (writes the current image bytes to a local file — metadata
and path only, like `get_artifact`), `list_images`, `delete_image`. Responses
include the public URL (`/a/<name>`, images `/i/<name>.<ext>`) so an agent can
hand the link straight to the user.

Large content never passes through the model context: `get_artifact` writes the
current content to a local file (default `<tmpdir>/artifacts/<name>.<ext>`,
override with `filePath`) and returns metadata plus the path; `save_artifact` /
`update_artifact` take either an inline `content` string or a `filePath` to
upload from — the edit loop is get → edit the file → update with `filePath`.
`update_artifact` also takes `expectedRev` — the rev from the `get_artifact`,
`save_artifact` or previous `update_artifact` result. When another agent updated
the artifact in between, the update fails with a version conflict and nothing is
written: re-read the artifact and re-apply the changes on top of the fresh
content, then update again with the new rev.
It uses `GET /api/artifacts/{name}/content` for raw bytes and falls back to the
public route on older servers, flagging injected-script content with a `warning`.

```sh
cd mcp && npm install && npm run build
ARTIFACTS_BASE_URL=https://… ARTIFACTS_TOKEN=… node mcp/dist/index.js
```

In practice it is launched by the wrapper `~/dotfiles/bin/mcp-artifacts`, which
reads the token from `~/.artifacts_token.secret`.

## Local run

```sh
cargo run --release            # or:
cargo build --release
ARTIFACTS_TOKEN=dev PORT=8080 ARTIFACTS_DATA_DIR=./data ./target/release/artifacts-server
```

## Deploy

The deploy repo is a separate checkout: `~/develop/work/minisklad-artifacts` —
kamal config plus the `./deploy` wrapper (image `morr507/artifacts` on Docker
Hub; TLS terminates at kamal-proxy; data on the host at
`/home/devops/artifacts/data`):

```sh
cd ~/develop/work/minisklad-artifacts && ./deploy
```

The build context is this repo (`../artifacts` relative to the deploy repo), so
uncommitted changes here ship as-is — no commit needed first. The image is
built remotely on the target server (native amd64, no qemu emulation).

## Tests

```sh
cargo test   # store unit tests + API integration tests (axum oneshot)
```
