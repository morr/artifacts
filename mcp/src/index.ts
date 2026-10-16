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
import { readFile } from "node:fs/promises";
import { extname } from "node:path";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const NAME_RULE = "must match ^[a-z0-9][a-z0-9-]{0,63}$";
const TEXTUAL_CONTENT_TYPES = [
  "text/",
  "application/json",
  "application/xml",
  "application/javascript",
  "application/yaml",
  "image/svg+xml",
];
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

function textual(contentType: string): boolean {
  const ct = contentType.split(";")[0].trim().toLowerCase();
  return TEXTUAL_CONTENT_TYPES.some((prefix) => ct.startsWith(prefix));
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

const server = new McpServer({ name: "artifacts", version: "0.2.0" });

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
      "Fetch an artifact's current content, metadata and public URL. Text content types come back as text, binary as base64.",
    inputSchema: { name: z.string().describe(`Artifact name, ${NAME_RULE}`) },
  },
  async ({ name }) => {
    const c = config();
    assertName(name);
    const meta = await fetchMeta(c, name);
    const resp = await fetch(`${c.baseUrl}/a/${name}`);
    if (!resp.ok) throw new ToolError(`artifacts API ${resp.status} ${resp.statusText}`);
    const buf = Buffer.from(await resp.arrayBuffer());
    const isText = textual(meta.meta.content_type);
    const result = {
      name,
      url: meta.url,
      contentType: meta.meta.content_type,
      updatedAt: meta.meta.updated_at,
      versionCount: meta.versions.length,
      encoding: isText ? "utf-8" : "base64",
      content: isText ? buf.toString("utf-8") : buf.toString("base64"),
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
      'contentType defaults to "text/html; charset=utf-8". Returns the public URL the artifact is served at.',
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      content: z.string().describe("Full artifact content (e.g. the whole HTML document)"),
      contentType: z.string().optional().describe('MIME type, default "text/html; charset=utf-8"'),
    },
  },
  async ({ name, content, contentType }) => {
    const c = config();
    assertName(name);
    const resp = await api(`/api/artifacts/${name}`, {
      ...c,
      method: "PUT",
      headers: {
        "Content-Type": contentType ?? "text/html; charset=utf-8",
        "X-Create-Only": "true",
      },
      body: content,
    });
    const result = (await resp.json()) as { name: string; url: string; created: boolean };
    return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
  },
);

server.registerTool(
  "update_artifact",
  {
    title: "Update artifact",
    description:
      "Replace the content of an EXISTING artifact; the previous version is archived and can be rolled back. " +
      "Fails if the artifact does not exist (use save_artifact then). contentType defaults to the artifact's current one.",
    inputSchema: {
      name: z.string().describe(`Artifact name, ${NAME_RULE}`),
      content: z.string().describe("Full new artifact content"),
      contentType: z.string().optional().describe("New MIME type; omit to keep the current one"),
    },
  },
  async ({ name, content, contentType }) => {
    const c = config();
    assertName(name);
    let type = contentType;
    if (!type) {
      const meta = await fetchMeta(c, name);
      type = meta.meta.content_type;
    }
    const resp = await api(`/api/artifacts/${name}`, {
      ...c,
      method: "PUT",
      headers: { "Content-Type": type, "X-Update-Only": "true" },
      body: content,
    });
    const result = (await resp.json()) as { name: string; url: string; created: boolean };
    return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
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
