# artifacts

Tiny artifact storage: a Rust web server (single static musl binary) that stores
files on its own filesystem, plus an MCP server for AI agents to work with them.

- **Public** (no auth): `GET /a/<name>` — serves the current version of an
  artifact with its stored content type (HTML renders in a browser); `GET /healthz`.
- **Everything else** requires `Authorization: Bearer $ARTIFACTS_TOKEN`:
  list, metadata, versions, upload/update, rollback, delete. There is no public
  listing and never will be — artifact names are only visible with the token.
- Updates archive the previous version (`versions/<timestamp>`), so anything can
  be rolled back.

Artifact names must match `^[a-z0-9][a-z0-9-]{0,63}$` — one safe path component.

## HTTP API

| Method | Path | Notes |
|---|---|---|
| GET | `/a/{name}` | public; current content |
| GET | `/healthz` | public; for kamal-proxy |
| GET | `/api/artifacts?prefix=` | list: name, content type, size, updated_at |
| GET | `/api/artifacts/{name}` | meta + version history + public URL |
| PUT | `/api/artifacts/{name}` | body = content; `X-Create-Only: true` → 409 if exists; `X-Update-Only: true` → 404 if missing; otherwise upsert. Content type taken from the request `Content-Type` |
| GET | `/api/artifacts/{name}/versions/{version}` | download a specific version |
| POST | `/api/artifacts/{name}/rollback/{version}` | make a version current (outgoing current is archived) |
| DELETE | `/api/artifacts/{name}` | remove the artifact with all versions |

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
  meta.json            # content_type, created_at, updated_at, size, sha256
  versions/2026-10-09T18-22-33-123Z       # archived content
  versions/2026-10-09T18-22-33-123Z.json  # its meta at save time
```

All writes are atomic (tmp + rename).

## MCP server (`mcp/`)

TypeScript MCP server over stdio with tools `list_artifacts`, `get_artifact`,
`save_artifact` (create-only), `update_artifact` (update-only), `list_versions`,
`rollback_artifact`, `delete_artifact`. Responses include the public URL
(`/a/<name>`) so an agent can hand the link straight to the user.

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

Deployed with kamal from the separate repo `~/develop/work/minisklad-artifacts`
(image `morr507/artifacts` on Docker Hub). TLS terminates at kamal-proxy.

## Tests

```sh
cargo test   # store unit tests + API integration tests (axum oneshot)
```
