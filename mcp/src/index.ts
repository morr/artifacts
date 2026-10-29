#!/usr/bin/env node
/**
 * MCP server for the artifacts storage (https://github.com/morr/artifacts).
 *
 * Tools: list/get/save/update artifacts, list versions, rollback, rename, delete;
 * plus image tools: get (download to a file), upload/update/list/delete images
 * (public URLs `/i/<name>` for embedding into artifacts).
 * Reads ARTIFACTS_BASE_URL and ARTIFACTS_TOKEN from the environment;
 * all tool failures come back as isError results with a readable message.
 *
 * Project namespaces: the storage groups artifacts and images by project —
 * URLs like /a/<project>/<name> and /i/<project>/<name>.png. The default
 * project is auto-detected from the working directory the app spawned this
 * server in (the session's project dir, slugified); every name-taking tool
 * accepts an explicit `project` override.
 */
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { basename, dirname, extname, join } from "node:path";
import { homedir, tmpdir } from "node:os";
import { createHash } from "node:crypto";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const NAME_RULE = "must match ^[a-z0-9][a-z0-9-]{0,63}$";
const IMAGE_TYPES_BY_EXT: Record<string, string> = {
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".webp": "image/webp",
  ".avif": "image/avif",
  ".svg": "image/svg+xml",
};

class ToolError extends Error {}

function config(): { baseUrl: string; token: string } {
  const baseUrl = process.env.ARTIFACTS_BASE_URL;
  const token = process.env.ARTIFACTS_TOKEN;
  if (!baseUrl || !token) {
    throw new ToolError(
      "ARTIFACTS_BASE_URL and ARTIFACTS_TOKEN are not set. They come from the wrapper " +
        "(~/dotfiles/bin/mcp-artifacts) which reads ~/.artifacts_token.secret.",
    );
  }
  return { baseUrl: baseUrl.replace(/\/+$/, ""), token };
}

// --- project namespace ---

function slugifyProject(raw: string): string | undefined {
  const slug = raw
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
  return /^[a-z0-9][a-z0-9-]*$/.test(slug) ? slug : undefined;
}

// The project new artifacts and images land in unless a call overrides it:
// ARTIFACTS_PROJECT wins; else the session directory this server was spawned
// in. $HOME, / and tmp dirs are no project — there the default is none,
// matching the legacy flat namespace.
const AUTO_PROJECT: string | undefined = (() => {
  const env = process.env.ARTIFACTS_PROJECT?.trim();
  if (env) return slugifyProject(env);
  const cwd = process.cwd();
  if (cwd === homedir() || cwd === "/" || cwd.startsWith(tmpdir())) return undefined;
  return slugifyProject(basename(cwd));
})();

// Explicit `project` param: "" means the legacy flat namespace on purpose;
// otherwise it must be a valid slug. Undefined → the auto-detected default.
function projectFor(explicit?: string): string | undefined {
  if (explicit !== undefined) {
    if (explicit === "") return undefined;
    if (!/^[a-z0-9][a-z0-9-]{0,63}$/.test(explicit)) {
      throw new ToolError(`invalid project "${explicit}": ${NAME_RULE}`);
    }
    return explicit;
  }
  return AUTO_PROJECT;
}

function projectHeaders(project?: string): Record<string, string> {
  return project ? { "X-Project": project } : {};
}

// Shared wording for the per-tool `project` params.
const PROJECT_PARAM = {
  describe:
    "Project namespace. Omitted: the session's project, auto-detected from the working directory " +
    '(e.g. ~/develop/work/minisklad → "minisklad"). "": the legacy flat namespace of artifacts ' +
    "stored before projects. Otherwise: a project slug. Reads and updates fall back to the legacy " +
    "namespace when the name is missing in the project; creates land in exactly the given namespace.",
};

async function api(
  path: string,
  init: RequestInit & { baseUrl: string; token: string },
): Promise<Response> {
  const resp = await fetch(`${init.baseUrl}${path}`, {
    ...init,
    headers: {
      Authorization: `Bearer ${init.token}`,
      ...(init.headers as Record<string, string> | undefined),
    },
  });
  if (!resp.ok) {
    let detail = `${resp.status} ${resp.statusText}`;
    try {
      const body = (await resp.json()) as { error?: string };
      if (body.error) detail = `${resp.status}: ${body.error}`;
    } catch {
      /* non-json error body, keep the status line */
    }
    throw new ToolError(`artifacts API ${detail}`);
  }
  return resp;
}

function assertName(name: string): string {
  if (!/^[a-z0-9][a-z0-9-]{0,63}$/.test(name)) {
    throw new ToolError(`invalid artifact name "${name}": ${NAME_RULE}`);
  }
  return name;
}

const sha256Hex = (data: Buffer): string => createHash("sha256").update(data).digest("hex");

// Extension for the default get_artifact download path; anything unknown is .bin.
const EXTENSIONS_BY_CONTENT_TYPE: Record<string, string> = {
  "text/html": ".html",
  "text/plain": ".txt",
  "text/markdown": ".md",
  "text/css": ".css",
  "text/csv": ".csv",
  "application/json": ".json",
  "application/xml": ".xml",
  "application/javascript": ".js",
  "application/yaml": ".yaml",
  "image/svg+xml": ".svg",
};

function defaultArtifactPath(project: string | undefined, name: string, contentType: string): string {
  const bare = contentType.split(";")[0].trim().toLowerCase();
  const ext = EXTENSIONS_BY_CONTENT_TYPE[bare] ?? ".bin";
  return join(tmpdir(), "artifacts", ...(project ? [project] : []), `${name}${ext}`);
}

// The inverse map for get_image's default download path: stored content type
// → file extension, matching the extension the server appends to /i/ URLs.
const EXTENSIONS_BY_IMAGE_TYPE: Record<string, string> = Object.fromEntries(
  Object.entries(IMAGE_TYPES_BY_EXT).map(([ext, ct]) => [ct, ext]),
);

function defaultImagePath(project: string | undefined, name: string, contentType: string): string {
  const bare = contentType.split(";")[0].trim().toLowerCase();
  const ext = EXTENSIONS_BY_IMAGE_TYPE[bare] ?? ".bin";
  return join(tmpdir(), "artifacts", ...(project ? [project] : []), `${name}${ext}`);
}

// Content for save/update: exactly one of an inline string or a local file.
// The file form keeps large content out of the model context — the usual edit
// loop is get_artifact (writes a file) → edit that file → update with filePath.
async function resolveContent(input: { content?: string; filePath?: string }): Promise<Buffer> {
  const given = [input.content, input.filePath].filter((v) => v !== undefined).length;
  if (given !== 1) throw new ToolError("provide exactly one of content or filePath");
  if (input.filePath !== undefined) {
    const path = input.filePath;
    const data = await readFile(path).catch((e: NodeJS.ErrnoException) => {
      throw new ToolError(`cannot read ${path}: ${e.message}`);
    });
    if (data.length === 0) throw new ToolError(`file is empty: ${path}`);
    return data;
  }
  return Buffer.from(input.content as string, "utf-8");
}

// Result rows returned to the model: plain JSON strings, compact.

interface ListRow {
  name: string;
  /** Project namespace; null for artifacts stored before projects. */
  project: string | null;
  content_type: string;
  size: number;
  updated_at: string;
}

interface VersionRow {
  version: string;
  content_type: string;
  size: number;
  sha256: string;
  saved_at: string;
}

interface MetaRow {
  rev: number;
  content_type: string;
  created_at: string;
  updated_at: string;
  size: number;
  sha256: string;
}

interface MetaResponse {
  name: string;
  /** The namespace the artifact was found in; null = legacy flat. */
  project: string | null;
  url: string;
  meta: MetaRow;
  versions: VersionRow[];
}

async function fetchMeta(
  c: { baseUrl: string; token: string },
  name: string,
  project?: string,
): Promise<MetaResponse> {
  const resp = await api(`/api/artifacts/${name}`, {
    ...c,
    method: "GET",
    headers: projectHeaders(project),
  });
  return (await resp.json()) as MetaResponse;
}

async function fetchImageMeta(
  c: { baseUrl: string; token: string },
  name: string,
  project?: string,
): Promise<MetaResponse> {
  const resp = await api(`/api/images/${name}`, {
    ...c,
    method: "GET",
    headers: projectHeaders(project),
  });
  return (await resp.json()) as MetaResponse;
}

// --- images ---

interface ImageSource {
  data: Buffer;
  /** Content type implied by the source (file extension, HTTP response), if any. */
  typeHint?: string;
}

async function resolveImageSource(input: {
  filePath?: string;
  sourceUrl?: string;
  contentBase64?: string;
}): Promise<ImageSource> {
  const given = [input.filePath, input.sourceUrl, input.contentBase64].filter((v) => v !== undefined).length;
  if (given !== 1) {
    throw new ToolError("provide exactly one image source: filePath, sourceUrl or contentBase64");
  }

  if (input.filePath !== undefined) {
    const path = input.filePath;
    const data = await readFile(path).catch((e: NodeJS.ErrnoException) => {
      throw new ToolError(`cannot read ${path}: ${e.message}`);
    });
    if (data.length === 0) throw new ToolError(`file is empty: ${path}`);
    return { data, typeHint: IMAGE_TYPES_BY_EXT[extname(path).toLowerCase()] };
  }

  if (input.sourceUrl !== undefined) {
    const url = input.sourceUrl;
    if (!/^https?:\/\//i.test(url)) throw new ToolError(`sourceUrl must be an http(s) URL: ${url}`);
    const resp = await fetch(url).catch((e: Error) => {
      throw new ToolError(`cannot fetch ${url}: ${e.message}`);
    });
    if (!resp.ok) throw new ToolError(`source fetch failed: ${resp.status} ${resp.statusText}`);
    const data = Buffer.from(await resp.arrayBuffer());
    if (data.length === 0) throw new ToolError(`source returned an empty body: ${url}`);
    // A generic or plainly wrong content type carries no signal — fall back to the default.
    const bare = resp.headers.get("content-type")?.split(";")[0].trim().toLowerCase();
    const hint = bare && bare !== "application/octet-stream" && bare !== "text/plain" ? bare : undefined;
    return { data, typeHint: hint };
  }

  const data = Buffer.from(input.contentBase64 as string, "base64");
  if (data.length === 0) throw new ToolError("contentBase64 is empty or not valid base64");
  return { data };
}

function assertImageType(contentType: string): string {
  const bare = contentType.split(";")[0].trim().toLowerCase();
  if (!/^image\//.test(bare)) {
    throw new ToolError(`image contentType must be image/* (png, jpeg, gif, webp, avif, svg), got "${contentType}"`);
  }
  return bare;
}

async function putImage(
  args: {
    name: string;
    filePath?: string;
    sourceUrl?: string;
    contentBase64?: string;
    contentType?: string;
    project?: string;
  },
  mode: "create" | "update",
) {
  const c = config();
  assertName(args.name);
  const project = projectFor(args.project);
  const { data, typeHint } = await resolveImageSource(args);
  let contentType = args.contentType ? assertImageType(args.contentType) : typeHint;
  if (!contentType) {
    contentType =
      mode === "update"
        ? (await fetchImageMeta(c, args.name, project)).meta.content_type
        : "image/png";
  }
  contentType = assertImageType(contentType);
  const resp = await api(`/api/images/${args.name}`, {
    ...c,
    method: "PUT",
    headers: {
      "Content-Type": contentType,
      [mode === "create" ? "X-Create-Only" : "X-Update-Only"]: "true",
      ...projectHeaders(project),
    },
    body: new Uint8Array(data),
  });
  const result = (await resp.json()) as {
    name: string;
    project: string | null;
    url: string;
    content_type: string;
    size: number;
    sha256: string;
    created: boolean;
  };
  return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }] };
}

const server = new McpServer({ name: "artifacts", version: "0.7.0" });

server.registerTool(
  "list_artifacts",
  {
    title: "List artifacts",
    description:
      "List artifacts in the storage, newest info per artifact. Optional prefix filters by name, e.g. prefix=\"report-\". " +
      "Optional project filters by namespace: omitted lists everything (legacy artifacts first, then each project); " +
      '"" lists only the legacy flat namespace; otherwise only that project.',
    inputSchema: {
      prefix: z.string().optional(),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ prefix, project }) => {
    const c = config();
    const params = new URLSearchParams();
    if (prefix) params.set("prefix", prefix);
    if (project !== undefined) params.set("project", project);
    const query = params.size ? `?${params}` : "";
    const resp = await api(`/api/artifacts${query}`, { ...c, method: "GET" });
    const items = (await resp.json()) as ListRow[];
    return { content: [{ type: "text", text: JSON.stringify(items, null, 2) }] };
  },
);

server.registerTool(
  "get_artifact",
  {
    title: "Get artifact",
    description:
      "Fetch an artifact's current content, metadata and public URL. The content is written to a local file " +
      "(default <tmpdir>/artifacts/[<project>/]<name>.<ext>, override with filePath — it is overwritten); the result carries " +
      "only metadata plus the path, never the content itself, so it can't be truncated. Read the file with file " +
      "tools; to publish edits, update_artifact with filePath — the content never has to pass through the model. " +
      "The result's rev is the concurrency token: pass it as expectedRev to update_artifact so an update by a " +
      "parallel agent can't be silently overwritten.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      filePath: z.string().optional().describe("Local file to write the content to; default <tmpdir>/artifacts/[<project>/]<name>.<ext>"),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, filePath, project }) => {
    const c = config();
    assertName(name);
    const ns = projectFor(project);
    const meta = await fetchMeta(c, name, ns);

    // Raw stored bytes via the authenticated content route. The public /a/
    // route (fallback for servers without it) serves text/html with the
    // live-reload script injected; the sha mismatch detects that precisely.
    let buf: Buffer;
    let warning: string | undefined;
    const raw = await api(`/api/artifacts/${name}/content`, {
      ...c,
      method: "GET",
      headers: projectHeaders(ns),
    }).catch(() => null);
    if (raw) {
      buf = Buffer.from(await raw.arrayBuffer());
    } else {
      const resp = await fetch(meta.url);
      if (!resp.ok) throw new ToolError(`artifacts API ${resp.status} ${resp.statusText}`);
      buf = Buffer.from(await resp.arrayBuffer());
      if (meta.meta.content_type.startsWith("text/html") && sha256Hex(buf) !== meta.meta.sha256) {
        warning =
          "this artifacts server predates the /content route, so the file was fetched from the public " +
          "route and its HTML carries the injected live-reload script; re-uploading it would store that " +
          "script — deploy the updated artifacts server before round-tripping HTML through get_artifact";
      }
    }

    const path = filePath ?? defaultArtifactPath(ns ?? undefined, name, meta.meta.content_type);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, buf);
    const result = {
      name,
      project: meta.project,
      url: meta.url,
      rev: meta.meta.rev,
      contentType: meta.meta.content_type,
      size: buf.length,
      sha256: sha256Hex(buf),
      updatedAt: meta.meta.updated_at,
      versionCount: meta.versions.length,
      path,
      ...(warning ? { warning } : {}),
    };
    return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
  },
);

server.registerTool(
  "get_image",
  {
    title: "Get image",
    description:
      "Download the current bytes of an image to a local file (default <tmpdir>/artifacts/[<project>/]<name>.<ext>, " +
      "override with filePath — it is overwritten); the result carries only metadata plus the path, never " +
      "the bytes themselves. Read the file with file tools or hand it to a vision tool. The reading " +
      "counterpart of upload_image: use it to inspect what an artifact embeds by its /i/<name> URL, instead " +
      "of fetching that URL over raw HTTP.",
    inputSchema: {
      name: z.string().describe(`Image name, ${NAME_RULE}`),
      filePath: z.string().optional().describe("Local file to write the bytes to; default <tmpdir>/artifacts/[<project>/]<name>.<ext>"),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, filePath, project }) => {
    const c = config();
    assertName(name);
    const ns = projectFor(project);
    const meta = await fetchImageMeta(c, name, ns);
    // The public /i/ route serves the raw stored bytes (no live-reload
    // injection — that only touches artifact HTML), so the stored sha256 must
    // match what came back.
    const resp = await fetch(meta.url);
    if (!resp.ok) throw new ToolError(`artifacts API ${resp.status} ${resp.statusText}`);
    const buf = Buffer.from(await resp.arrayBuffer());
    if (buf.length === 0) throw new ToolError(`artifacts API returned an empty body for ${meta.url}`);
    const sha256 = sha256Hex(buf);
    const path = filePath ?? defaultImagePath(ns ?? undefined, name, meta.meta.content_type);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, buf);
    const result = {
      name,
      project: meta.project,
      url: meta.url,
      rev: meta.meta.rev,
      contentType: meta.meta.content_type,
      size: buf.length,
      sha256,
      updatedAt: meta.meta.updated_at,
      versionCount: meta.versions.length,
      path,
      ...(sha256 !== meta.meta.sha256
        ? { warning: `sha256 mismatch: stored ${meta.meta.sha256}, downloaded ${sha256}` }
        : {}),
    };
    return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
  },
);

server.registerTool(
  "save_artifact",
  {
    title: "Save (create) artifact",
    description:
      "Create a NEW artifact in the storage. Fails if the name already exists in the target namespace (use update_artifact then). " +
      "The artifact lands in exactly the given project namespace — the same name in another project or in the legacy " +
      "flat namespace is a different artifact and does not block the create. " +
      "Content comes from exactly one of content (inline string) or filePath (local file). " +
      'contentType defaults to "text/html; charset=utf-8". Returns the public URL the artifact is served at ' +
      "and the rev to pass as expectedRev to follow-up update_artifact calls.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      content: z.string().optional().describe("Full artifact content (e.g. the whole HTML document)"),
      filePath: z.string().optional().describe("Local file to upload the content from"),
      contentType: z.string().optional().describe('MIME type, default "text/html; charset=utf-8"'),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, content, filePath, contentType, project }) => {
    const c = config();
    assertName(name);
    const ns = projectFor(project);
    const data = await resolveContent({ content, filePath });
    const resp = await api(`/api/artifacts/${name}`, {
      ...c,
      method: "PUT",
      headers: {
        "Content-Type": contentType ?? "text/html; charset=utf-8",
        "X-Create-Only": "true",
        ...projectHeaders(ns),
      },
      body: new Uint8Array(data),
    });
    const result = (await resp.json()) as {
      name: string;
      project: string | null;
      url: string;
      rev?: number;
      created: boolean;
    };
    return {
      content: [{ type: "text", text: JSON.stringify({ ...result, size: data.length, sha256: sha256Hex(data) }, null, 2) }],
    };
  },
);

server.registerTool(
  "update_artifact",
  {
    title: "Update artifact",
    description:
      "Replace the content of an EXISTING artifact; the previous version is archived and can be rolled back. " +
      "Fails if the artifact does not exist in the target namespace (use save_artifact then). Content comes from exactly one of content " +
      "(inline string) or filePath (local file) — for editing an existing artifact prefer the loop get_artifact → " +
      "edit the file → update_artifact with filePath. contentType defaults to the artifact's current one. " +
      "expectedRev is the concurrency guard: pass the rev from the get_artifact, save_artifact or last " +
      "update_artifact result; if it no longer matches (another agent updated the artifact in the meantime), " +
      "the update fails with a version conflict and nothing is written — re-read the artifact with get_artifact, " +
      "re-apply your changes on top of the fresh content, and update again with the new rev. Several agents may " +
      "work on the same artifact, so always pass expectedRev.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      content: z.string().optional().describe("Full new artifact content"),
      filePath: z.string().optional().describe("Local file to upload the new content from"),
      contentType: z.string().optional().describe("New MIME type; omit to keep the current one"),
      expectedRev: z
        .number()
        .int()
        .optional()
        .describe("Rev from get_artifact / save_artifact / the last update_artifact result; update fails with a version conflict if it is no longer current"),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, content, filePath, contentType, expectedRev, project }) => {
    const c = config();
    assertName(name);
    const ns = projectFor(project);
    const data = await resolveContent({ content, filePath });
    let type = contentType;
    if (!type) {
      const meta = await fetchMeta(c, name, ns);
      type = meta.meta.content_type;
    }
    const resp = await api(`/api/artifacts/${name}`, {
      ...c,
      method: "PUT",
      headers: {
        "Content-Type": type,
        "X-Update-Only": "true",
        ...(expectedRev !== undefined ? { "X-Expected-Rev": String(expectedRev) } : {}),
        ...projectHeaders(ns),
      },
      body: new Uint8Array(data),
    });
    const result = (await resp.json()) as {
      name: string;
      project: string | null;
      url: string;
      rev?: number;
      created: boolean;
    };
    return {
      content: [{ type: "text", text: JSON.stringify({ ...result, size: data.length, sha256: sha256Hex(data) }, null, 2) }],
    };
  },
);

server.registerTool(
  "list_versions",
  {
    title: "List artifact versions",
    description: "Show metadata and the archived version history of an artifact.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, project }) => {
    const c = config();
    assertName(name);
    const meta = await fetchMeta(c, name, projectFor(project));
    return { content: [{ type: "text", text: JSON.stringify(meta, null, 2) }] };
  },
);

server.registerTool(
  "rollback_artifact",
  {
    title: "Rollback artifact",
    description:
      "Roll an artifact back to a previous version (see list_versions). The outgoing current version is itself archived.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      version: z.string().describe("Version id from list_versions, e.g. 2026-10-09T18-22-33-123Z"),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, version, project }) => {
    const c = config();
    assertName(name);
    const ns = projectFor(project);
    const resp = await api(`/api/artifacts/${name}/rollback/${version}`, {
      ...c,
      method: "POST",
      headers: projectHeaders(ns),
    });
    const meta = (await resp.json()) as MetaRow;
    return { content: [{ type: "text", text: JSON.stringify({ name, project: ns ?? null, rolledBackTo: version, meta }, null, 2) }] };
  },
);

server.registerTool(
  "rename_artifact",
  {
    title: "Rename artifact",
    description:
      "Rename an artifact: the public URL changes to the new name; the old URL keeps answering " +
      "with a redirect to it (until something is created under the old name). " +
      "The rename stays inside the artifact's namespace (project first, legacy fallback); the target " +
      "is looked up in the same namespace. " +
      "Content, version history and timestamps are preserved; fails if the new name already exists there.",
    inputSchema: {
      name: z.string().describe(`Current artifact name, ${NAME_RULE}`),
      newName: z.string().describe(`New artifact name, ${NAME_RULE}`),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, newName, project }) => {
    const c = config();
    assertName(name);
    assertName(newName);
    const ns = projectFor(project);
    const resp = await api(`/api/artifacts/${name}/rename/${newName}`, {
      ...c,
      method: "POST",
      headers: projectHeaders(ns),
    });
    const result = (await resp.json()) as { old_name: string; name: string; project: string | null; url: string };
    return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
  },
);

server.registerTool(
  "delete_artifact",
  {
    title: "Delete artifact",
    description: "Delete an artifact together with all its versions. Irreversible.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, project }) => {
    const c = config();
    assertName(name);
    const ns = projectFor(project);
    await api(`/api/artifacts/${name}`, {
      ...c,
      method: "DELETE",
      headers: projectHeaders(ns),
    });
    return { content: [{ type: "text", text: JSON.stringify({ name, project: ns ?? null, deleted: true }, null, 2) }] };
  },
);

server.registerTool(
  "upload_image",
  {
    title: "Upload image",
    description:
      "Upload a NEW image (screenshot, photo, generated picture) to the storage and get its public URL " +
      "(<base>/i/[<project>/]<name>[.<ext>]) for embedding into artifacts, e.g. <img src>. Fails if the name already " +
      "exists in the target namespace (use update_image then) — the same name in another project is a different " +
      "image and does not block the upload. Provide exactly one source: filePath (local image file), sourceUrl " +
      "(http(s) URL to download) or contentBase64 (raw bytes encoded as base64 — prefer filePath/sourceUrl " +
      'for anything large). contentType must be image/*; default: from the file extension or the source ' +
      'response, else "image/png". Uploading several images for one page? Use upload_images instead — ' +
      'one call instead of N.',
    inputSchema: {
      name: z.string().describe(`Image name, ${NAME_RULE}`),
      filePath: z.string().optional().describe("Local image file to read the bytes from"),
      sourceUrl: z.string().optional().describe("http(s) URL to download the image from"),
      contentBase64: z.string().optional().describe("Image bytes encoded as base64"),
      contentType: z.string().optional().describe('MIME type, must be image/*, e.g. "image/png"'),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async (args) => putImage(args, "create"),
);

server.registerTool(
  "update_image",
  {
    title: "Update image",
    description:
      "Replace the bytes of an EXISTING image, keeping its public URL (<base>/i/[<project>/]<name>); the previous " +
      "version is archived. Fails if the image does not exist in the target namespace (use upload_image then). Sources are the " +
      "same as upload_image (exactly one of filePath, sourceUrl, contentBase64); contentType default: " +
      "from the source, else the image's current type. Uploading several images for one page? Use " +
      "upload_images instead — one call instead of N.",
    inputSchema: {
      name: z.string().describe(`Image name, ${NAME_RULE}`),
      filePath: z.string().optional().describe("Local image file to read the bytes from"),
      sourceUrl: z.string().optional().describe("http(s) URL to download the image from"),
      contentBase64: z.string().optional().describe("Image bytes encoded as base64"),
      contentType: z.string().optional().describe('MIME type, must be image/*, e.g. "image/png"'),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async (args) => putImage(args, "update"),
);

server.registerTool(
  "upload_images",
  {
    title: "Upload images (batch)",
    description:
      "Upload SEVERAL new images in one call and get their public URLs (<base>/i/[<project>/]<name>[.<ext>]) for embedding " +
      "into artifacts, e.g. <img src>. The batch alternative to repeated upload_image calls: one tool " +
      "round-trip instead of N — use it whenever more than a couple of images go into the same page. " +
      "Fails per item, not as a whole: each result row carries either the URL or an error (e.g. a name " +
      "that already exists), successful items still land — retry only the failed rows. Each image takes " +
      "the same sources as upload_image (exactly one of filePath, sourceUrl, contentBase64); contentType " +
      "defaults from the extension or the source response, else image/png. Max 50 images and 10 MB of " +
      "total bytes per call (the request body limit) — split bigger sets into several calls.",
    inputSchema: {
      images: z
        .array(
          z.object({
            name: z.string().describe(`Image name, ${NAME_RULE}`),
            filePath: z.string().optional().describe("Local image file to read the bytes from"),
            sourceUrl: z.string().optional().describe("http(s) URL to download the image from"),
            contentBase64: z.string().optional().describe("Image bytes encoded as base64"),
            contentType: z.string().optional().describe('MIME type, must be image/*, e.g. "image/png"; default: from the extension/source, else "image/png"'),
            project: z.string().optional().describe("Project namespace override for this image; default: the batch-level project"),
          }),
        )
        .min(1)
        .max(50)
        .describe("Images to upload, in the order the result rows come back"),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
      replace: z
        .boolean()
        .optional()
        .describe("Replace images that already exist in the target namespace (the previous version is archived); default false — an existing name is a per-item error"),
    },
  },
  async ({ images, project, replace }) => {
    const c = config();
    const ns = projectFor(project);
    const resolved = await Promise.all(
      images.map(async (item) => {
        assertName(item.name);
        const given = [item.filePath, item.sourceUrl, item.contentBase64].filter((v) => v !== undefined).length;
        if (given !== 1) {
          throw new ToolError(`image "${item.name}": provide exactly one source: filePath, sourceUrl or contentBase64`);
        }
        const itemProject = item.project !== undefined ? projectFor(item.project) : ns;
        const { data, typeHint } = await resolveImageSource(item);
        const contentType = assertImageType(item.contentType ?? typeHint ?? "image/png");
        return { name: item.name, project: itemProject, content: data.toString("base64"), contentType };
      }),
    );
    const resp = await api("/api/images/batch", {
      ...c,
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ replace: replace ?? false, project: ns, images: resolved }),
    }).catch((e: Error) => {
      if (/404/.test(e.message)) {
        throw new ToolError(
          e.message +
            " — this artifacts server predates the batch route; deploy the updated server, " +
            "or upload the images one by one with upload_image",
        );
      }
      throw e;
    });
    const batch = (await resp.json()) as { uploaded: number; failed: number; results: Array<Record<string, unknown>> };
    return { content: [{ type: "text", text: JSON.stringify(batch, null, 2) }] };
  },
);

server.registerTool(
  "list_images",
  {
    title: "List images",
    description:
      "List uploaded images with their public URLs (<base>/i/<name>), newest info per image. " +
      'Optional prefix filters by name, e.g. prefix="report-1-". Optional project filters by namespace: ' +
      "omitted lists everything (legacy images first, then each project); " +
      '"" lists only the legacy flat namespace; otherwise only that project.',
    inputSchema: {
      prefix: z.string().optional(),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ prefix, project }) => {
    const c = config();
    const params = new URLSearchParams();
    if (prefix) params.set("prefix", prefix);
    if (project !== undefined) params.set("project", project);
    const query = params.size ? `?${params}` : "";
    const resp = await api(`/api/images${query}`, { ...c, method: "GET" });
    const items = (await resp.json()) as ListRow[];
    return { content: [{ type: "text", text: JSON.stringify(items, null, 2) }] };
  },
);

server.registerTool(
  "delete_image",
  {
    title: "Delete image",
    description:
      "Delete an image together with its versions. Irreversible; every artifact embedding it by URL " +
      "will show a broken image.",
    inputSchema: {
      name: z.string().describe(`Image name, ${NAME_RULE}`),
      project: z.string().optional().describe(PROJECT_PARAM.describe),
    },
  },
  async ({ name, project }) => {
    const c = config();
    assertName(name);
    const ns = projectFor(project);
    await api(`/api/images/${name}`, {
      ...c,
      method: "DELETE",
      headers: projectHeaders(ns),
    });
    return { content: [{ type: "text", text: JSON.stringify({ name, project: ns ?? null, deleted: true }, null, 2) }] };
  },
);

async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch((err) => {
  console.error("fatal:", err);
  process.exit(1);
});
