#!/usr/bin/env node
/**
 * MCP server for the artifacts storage (https://github.com/morr/artifacts).
 *
 * Tools: list/get/save/update artifacts, list versions, rollback, rename, delete;
 * plus image tools: upload/update/list/delete images (public URLs `/i/<name>`
 * for embedding into artifacts).
 * Reads ARTIFACTS_BASE_URL and ARTIFACTS_TOKEN from the environment;
 * all tool failures come back as isError results with a readable message.
 */
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, extname, join } from "node:path";
import { tmpdir } from "node:os";
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

function defaultArtifactPath(name: string, contentType: string): string {
  const bare = contentType.split(";")[0].trim().toLowerCase();
  return join(tmpdir(), "artifacts", `${name}${EXTENSIONS_BY_CONTENT_TYPE[bare] ?? ".bin"}`);
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
  url: string;
  meta: MetaRow;
  versions: VersionRow[];
}

async function fetchMeta(c: { baseUrl: string; token: string }, name: string): Promise<MetaResponse> {
  const resp = await api(`/api/artifacts/${name}`, { ...c, method: "GET" });
  return (await resp.json()) as MetaResponse;
}

async function fetchImageMeta(c: { baseUrl: string; token: string }, name: string): Promise<MetaResponse> {
  const resp = await api(`/api/images/${name}`, { ...c, method: "GET" });
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
  },
  mode: "create" | "update",
) {
  const c = config();
  assertName(args.name);
  const { data, typeHint } = await resolveImageSource(args);
  let contentType = args.contentType ? assertImageType(args.contentType) : typeHint;
  if (!contentType) {
    contentType =
      mode === "update"
        ? (await fetchImageMeta(c, args.name)).meta.content_type
        : "image/png";
  }
  contentType = assertImageType(contentType);
  const resp = await api(`/api/images/${args.name}`, {
    ...c,
    method: "PUT",
    headers: {
      "Content-Type": contentType,
      [mode === "create" ? "X-Create-Only" : "X-Update-Only"]: "true",
    },
    body: new Uint8Array(data),
  });
  const result = (await resp.json()) as {
    name: string;
    url: string;
    content_type: string;
    size: number;
    sha256: string;
    created: boolean;
  };
  return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }] };
}

const server = new McpServer({ name: "artifacts", version: "0.4.0" });

server.registerTool(
  "list_artifacts",
  {
    title: "List artifacts",
    description:
      "List artifacts in the storage, newest info per artifact. Optional prefix filters by name, e.g. prefix=\"report-\".",
    inputSchema: { prefix: z.string().optional() },
  },
  async ({ prefix }) => {
    const c = config();
    const resp = await api(`/api/artifacts${prefix ? `?prefix=${encodeURIComponent(prefix)}` : ""}`, {
      ...c,
      method: "GET",
    });
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
      "(default <tmpdir>/artifacts/<name>.<ext>, override with filePath — it is overwritten); the result carries " +
      "only metadata plus the path, never the content itself, so it can't be truncated. Read the file with file " +
      "tools; to publish edits, update_artifact with filePath — the content never has to pass through the model. " +
      "The result's rev is the concurrency token: pass it as expectedRev to update_artifact so an update by a " +
      "parallel agent can't be silently overwritten.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      filePath: z.string().optional().describe("Local file to write the content to; default <tmpdir>/artifacts/<name>.<ext>"),
    },
  },
  async ({ name, filePath }) => {
    const c = config();
    assertName(name);
    const meta = await fetchMeta(c, name);

    // Raw stored bytes via the authenticated content route. The public /a/
    // route (fallback for servers without it) serves text/html with the
    // live-reload script injected; the sha mismatch detects that precisely.
    let buf: Buffer;
    let warning: string | undefined;
    const raw = await api(`/api/artifacts/${name}/content`, { ...c, method: "GET" }).catch(() => null);
    if (raw) {
      buf = Buffer.from(await raw.arrayBuffer());
    } else {
      const resp = await fetch(`${c.baseUrl}/a/${name}`);
      if (!resp.ok) throw new ToolError(`artifacts API ${resp.status} ${resp.statusText}`);
      buf = Buffer.from(await resp.arrayBuffer());
      if (meta.meta.content_type.startsWith("text/html") && sha256Hex(buf) !== meta.meta.sha256) {
        warning =
          "this artifacts server predates the /content route, so the file was fetched from the public " +
          "route and its HTML carries the injected live-reload script; re-uploading it would store that " +
          "script — deploy the updated artifacts server before round-tripping HTML through get_artifact";
      }
    }

    const path = filePath ?? defaultArtifactPath(name, meta.meta.content_type);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, buf);
    const result = {
      name,
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
  "save_artifact",
  {
    title: "Save (create) artifact",
    description:
      "Create a NEW artifact in the storage. Fails if the name already exists (use update_artifact then). " +
      "Content comes from exactly one of content (inline string) or filePath (local file). " +
      'contentType defaults to "text/html; charset=utf-8". Returns the public URL the artifact is served at ' +
      "and the rev to pass as expectedRev to follow-up update_artifact calls.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      content: z.string().optional().describe("Full artifact content (e.g. the whole HTML document)"),
      filePath: z.string().optional().describe("Local file to upload the content from"),
      contentType: z.string().optional().describe('MIME type, default "text/html; charset=utf-8"'),
    },
  },
  async ({ name, content, filePath, contentType }) => {
    const c = config();
    assertName(name);
    const data = await resolveContent({ content, filePath });
    const resp = await api(`/api/artifacts/${name}`, {
      ...c,
      method: "PUT",
      headers: {
        "Content-Type": contentType ?? "text/html; charset=utf-8",
        "X-Create-Only": "true",
      },
      body: new Uint8Array(data),
    });
    const result = (await resp.json()) as { name: string; url: string; rev?: number; created: boolean };
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
      "Fails if the artifact does not exist (use save_artifact then). Content comes from exactly one of content " +
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
    },
  },
  async ({ name, content, filePath, contentType, expectedRev }) => {
    const c = config();
    assertName(name);
    const data = await resolveContent({ content, filePath });
    let type = contentType;
    if (!type) {
      const meta = await fetchMeta(c, name);
      type = meta.meta.content_type;
    }
    const resp = await api(`/api/artifacts/${name}`, {
      ...c,
      method: "PUT",
      headers: {
        "Content-Type": type,
        "X-Update-Only": "true",
        ...(expectedRev !== undefined ? { "X-Expected-Rev": String(expectedRev) } : {}),
      },
      body: new Uint8Array(data),
    });
    const result = (await resp.json()) as { name: string; url: string; rev?: number; created: boolean };
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
    inputSchema: { name: z.string().describe(`Artifact name, ${NAME_RULE}`) },
  },
  async ({ name }) => {
    const c = config();
    assertName(name);
    const meta = await fetchMeta(c, name);
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
    },
  },
  async ({ name, version }) => {
    const c = config();
    assertName(name);
    const resp = await api(`/api/artifacts/${name}/rollback/${version}`, { ...c, method: "POST" });
    const meta = (await resp.json()) as MetaRow;
    return { content: [{ type: "text", text: JSON.stringify({ name, rolledBackTo: version, meta }, null, 2) }] };
  },
);

server.registerTool(
  "rename_artifact",
  {
    title: "Rename artifact",
    description:
      "Rename an artifact: the public URL changes to the new name; the old URL keeps answering " +
      "with a redirect to it (until something is created under the old name). " +
      "Content, version history and timestamps are preserved; fails if the new name already exists.",
    inputSchema: {
      name: z.string().describe(`Current artifact name, ${NAME_RULE}`),
      newName: z.string().describe(`New artifact name, ${NAME_RULE}`),
    },
  },
  async ({ name, newName }) => {
    const c = config();
    assertName(name);
    assertName(newName);
    const resp = await api(`/api/artifacts/${name}/rename/${newName}`, { ...c, method: "POST" });
    const result = (await resp.json()) as { old_name: string; name: string; url: string };
    return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
  },
);

server.registerTool(
  "delete_artifact",
  {
    title: "Delete artifact",
    description: "Delete an artifact together with all its versions. Irreversible.",
    inputSchema: { name: z.string().describe(`Artifact name, ${NAME_RULE}`) },
  },
  async ({ name }) => {
    const c = config();
    assertName(name);
    await api(`/api/artifacts/${name}`, { ...c, method: "DELETE" });
    return { content: [{ type: "text", text: JSON.stringify({ name, deleted: true }, null, 2) }] };
  },
);

server.registerTool(
  "upload_image",
  {
    title: "Upload image",
    description:
      "Upload a NEW image (screenshot, photo, generated picture) to the storage and get its public URL " +
      "(<base>/i/<name>) for embedding into artifacts, e.g. <img src>. Fails if the name already exists " +
      "(use update_image then). Provide exactly one source: filePath (local image file), sourceUrl " +
      "(http(s) URL to download) or contentBase64 (raw bytes encoded as base64 — prefer filePath/sourceUrl " +
      'for anything large). contentType must be image/*; default: from the file extension or the source ' +
      'response, else "image/png".',
    inputSchema: {
      name: z.string().describe(`Image name, ${NAME_RULE}`),
      filePath: z.string().optional().describe("Local image file to read the bytes from"),
      sourceUrl: z.string().optional().describe("http(s) URL to download the image from"),
      contentBase64: z.string().optional().describe("Image bytes encoded as base64"),
      contentType: z.string().optional().describe('MIME type, must be image/*, e.g. "image/png"'),
    },
  },
  async (args) => putImage(args, "create"),
);

server.registerTool(
  "update_image",
  {
    title: "Update image",
    description:
      "Replace the bytes of an EXISTING image, keeping its public URL (<base>/i/<name>); the previous " +
      "version is archived. Fails if the image does not exist (use upload_image then). Sources are the " +
      "same as upload_image (exactly one of filePath, sourceUrl, contentBase64); contentType default: " +
      "from the source, else the image's current type.",
    inputSchema: {
      name: z.string().describe(`Image name, ${NAME_RULE}`),
      filePath: z.string().optional().describe("Local image file to read the bytes from"),
      sourceUrl: z.string().optional().describe("http(s) URL to download the image from"),
      contentBase64: z.string().optional().describe("Image bytes encoded as base64"),
      contentType: z.string().optional().describe('MIME type, must be image/*, e.g. "image/png"'),
    },
  },
  async (args) => putImage(args, "update"),
);

server.registerTool(
  "list_images",
  {
    title: "List images",
    description:
      "List uploaded images with their public URLs (<base>/i/<name>), newest info per image. " +
      'Optional prefix filters by name, e.g. prefix="report-1-".',
    inputSchema: { prefix: z.string().optional() },
  },
  async ({ prefix }) => {
    const c = config();
    const resp = await api(`/api/images${prefix ? `?prefix=${encodeURIComponent(prefix)}` : ""}`, {
      ...c,
      method: "GET",
    });
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
    inputSchema: { name: z.string().describe(`Image name, ${NAME_RULE}`) },
  },
  async ({ name }) => {
    const c = config();
    assertName(name);
    await api(`/api/images/${name}`, { ...c, method: "DELETE" });
    return { content: [{ type: "text", text: JSON.stringify({ name, deleted: true }, null, 2) }] };
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
