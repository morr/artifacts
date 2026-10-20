---
name: artifact-builder
description: Create, update, and publish self-contained HTML artifacts to the self-hosted artifacts storage via the `artifacts` MCP server. Use whenever the user asks for an artifact, a published page/dashboard/report/visualization with a shareable link, or wants to update, roll back, or delete an existing artifact. Default look is the merged report system inside (stone/slate editorial identity, Catppuccin Frappé code plates, ELK mermaid diagrams); the Claude/Anthropic brand style is an alternative, used only on explicit request.
---

# Artifact Builder

Turn a request into a published artifact with a public URL. The storage is the self-hosted artifacts server (repo `~/develop/work/artifacts`); every read and write goes through the `artifacts` MCP tools (`mcp__artifacts__*`). Never write artifact files into repos, dotfiles, or `/tmp` as a storage substitute — the artifact lives in the storage; locally it exists only as scratch files. **Work with artifacts as files and keep whole documents out of the conversation**: author the HTML into a scratch file, publish from it with `filePath`, iterate with `Edit` — holding or re-emitting a full document inline is both the truncation risk that once broke large artifacts and a needless context spend.

## Workflow

1. **Design before code.** The default look is the merged report system in [The default design](#the-default-design) below — every artifact ships in it unless the user explicitly asks for the Claude brand or another style. Start from the scaffold there, then run the formation two-pass: a compact plan (palette roles, type roles, one-sentence layout concept), reviewed against the anti-defaults list, before any code. [design.md](design.md) stays the craft authority for everything the scaffold doesn't fix.
2. **Pick a name.** An English kebab-case slug matching `^[a-z0-9][a-z0-9-]{0,63}$`, descriptive of the content (`rails-cache-report`, not `page-1`). If a collision is plausible, check with `list_artifacts` first.
3. **Build one self-contained HTML document.** All CSS inline in one `<style>` block; no local file references. The only external resources are the two CDN dependencies of the default design (mermaid+ELK, Shiki) and images already uploaded to the storage (`/i/<name>` URLs from step 4); all from `cdn.jsdelivr.net` — the reachable CDN (`cdn.tailwindcss.com` is dead from this network, never build on it). The page must stay usable when a CDN fails: diagrams degrade to source text, code loses colors, styling survives. Body limit is 10 MB. User-facing text is Russian (`<html lang="ru">`, `<title>` set); identifiers and code comments are English. Author the document into a scratch file (`<tmpdir>/artifacts/<name>.html` — the same place `get_artifact` puts downloads), not into the conversation: it is published from the file (step 5), and every later change is an `Edit` against it, never a re-emitted full document.
4. **Attach images when the page needs them.** A screenshot, a photo, a generated picture: upload each one *before* publishing the page — `upload_image` with one source (`filePath` for a local file, `sourceUrl` and the tool downloads it, or `contentBase64`) and a kebab-case name conventionally prefixed with the artifact slug (`<artifact-slug>-<what-it-shows>`). The response carries the public image URL (`<base>/i/<name>`) — embed that URL in `<img src>`. `update_image` replaces an image's bytes under the same URL; `delete_image` removes it. Images live in their own namespace, separate from artifacts: deleting an artifact never deletes its images, so when retiring an artifact, clean up its images too (`list_images` with the `<artifact-slug>-` prefix).
5. **Publish.** `save_artifact` for a new name (fails if the name already exists), `update_artifact` for an existing one (fails if missing; archives the previous version). Pass the scratch file as `filePath`; inline `content` is a fallback for small one-shot content only. Omit `contentType` for HTML — `text/html; charset=utf-8` is the default.
6. **Open the fresh page in the user's Chrome.** Right after a successful first publish — `save_artifact` has returned the URL — open it for the user: `open -a "Google Chrome" "<base>/a/<name>"` (macOS). Creation only: `update_artifact`, `rollback_artifact` and image uploads never re-open the page — the tab the user already has open picks the changes up via live reload. Best effort: if Chrome is missing or the command fails, just give the link in the reply.
7. **Verify the page, not the upload.** A tool response that returns the public URL (`<base>/a/<name>`, no auth needed to view) is the success signal — the artifact is stored and served. Never re-check that fact afterwards: no `curl`/HEAD probes of the URL, no `get_artifact`/`list_versions` round-trips "to be sure" — a failed call returns an error, not a URL. What does need checking is the page itself. Before publishing, cross-check markup against the stylesheet: every class used in the body must have a matching rule in the `<style>` block, and repeated devices (badges, labels, number spans) are styled by their own class, never scoped to a single parent. Every `<img src>` must point at a published `/i/<name>` URL. For non-trivial layouts, take a quick browser screenshot of the published URL — that is visual QA of the design (overlaps, clipped text, broken SVG), not a check that the upload happened. The server rarely answers a first load with an empty document — reload the tab once before diagnosing.
8. **Iterate on feedback.** To edit an existing artifact, use the file loop — never re-type large content inline: `get_artifact` writes the current HTML to a local scratch file (default `<tmpdir>/artifacts/<name>.html`) and returns only metadata plus the path; edit that file with file tools (`Read` with offset/limit, `Grep`, `Edit` — no full re-reads), then publish with `update_artifact` passing `filePath`. Keep `content` inline only for small artifacts. (This is the editing use of `get_artifact`; the never-verify rule from step 7 still stands.) To restore a previous state: `list_versions`, then `rollback_artifact` with the version id — the rollback is itself archived, nothing is lost. `delete_artifact` is irreversible: confirm with the user first, every time.

## MCP tools (server `artifacts`)

| Tool | Arguments | Notes |
| --- | --- | --- |
| `list_artifacts` | `prefix?` | newest info per artifact; prefix filters by name |
| `get_artifact` | `name`, `filePath?` | writes the current content to a local file (default `<tmpdir>/artifacts/<name>.<ext>`, overwritten) and returns meta + the path — never the content inline, so the result can't be truncated |
| `save_artifact` | `name`, one of `content` / `filePath`, `contentType?` | create-only; exactly one content source |
| `update_artifact` | `name`, one of `content` / `filePath`, `contentType?` | update-only; exactly one content source; omit `contentType` to keep the current one |
| `list_versions` | `name` | metadata + archived version history |
| `rollback_artifact` | `name`, `version` | version id from `list_versions`, e.g. `2026-10-09T18-22-33-123Z` |
| `rename_artifact` | `name`, `newName` | changes the name and the public URL; content, history and timestamps are preserved, the old URL answers with a redirect |
| `delete_artifact` | `name` | irreversible — ask the user first |
| `upload_image` | `name`, one of `filePath` / `sourceUrl` / `contentBase64`, `contentType?` | create-only; exactly one source; returns the public image URL `<base>/i/<name>` for `<img src>` |
| `update_image` | same sources | update-only; replaces the bytes, keeps the URL, archives the previous version |
| `list_images` | `prefix?` | uploaded images with their URLs; prefix filters by name |
| `delete_image` | `name` | irreversible — ask the user first; artifacts embedding it show a broken image |

## The default design

Two layers, one page. The **identity layer** (how it looks) is the stone/slate editorial scaffold below — adapted from Matt Pocock's `improve-codebase-architecture` HTML-report format (the local `~/.agents/skills/improve-codebase-architecture` variant, not the GitHub original). The **formation layer** (what carries weight, what gets emphasized, how the page is structured and critiqued) is Anthropic's `frontend-design` ([design.md](design.md)). The layers stay separable: formation advice never pulls in the Claude brand palette, fonts, or devices. Proven **style** reference: `8703-plan-matt-claude` — style only: its content lags behind the live plan, and sibling copies of an artifact drift through parallel sessions, so when restyling an existing artifact always take the content from the target artifact itself.

### Scaffold

```html
<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>…</title>
<script type="module">
  import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";
  const elkLayouts = (await import("https://cdn.jsdelivr.net/npm/@mermaid-js/layout-elk/dist/mermaid-layout-elk.esm.min.mjs")).default;
  try {
    mermaid.registerLayoutLoaders(elkLayouts);
  } catch (e) {
    mermaid.registerLayoutLoaders({ register: elkLayouts.register });
  }
  mermaid.initialize({
    startOnLoad: true,
    securityLevel: "loose",
    theme: "neutral",
    flowchart: {
      defaultRenderer: "elk",
      useMaxWidth: false,
      diagramPadding: 24,
      nodeSpacing: 64,
      rankSpacing: 96,
      padding: 14,
    },
  });
</script>
<style>
  :root {
    font-size: 18px;
    --bg: #fafaf9; --surface: #ffffff; --ink: #0f172a; --muted: #64748b;
    --line: #e2e8f0; --accent: #059669; --warn: #b45309; --leak: #dc2626;
    --soft: #f1f0ec;
  }
  @media (prefers-color-scheme: dark) {
    :root:not([data-theme="light"]) {
      --bg: #0b1120; --surface: #131c2e; --ink: #e8eef8; --muted: #94a3b8;
      --line: #26334a; --accent: #34d399; --warn: #fbbf24; --leak: #f87171;
      --soft: #182338;
    }
  }
  :root[data-theme="dark"] {
    --bg: #0b1120; --surface: #131c2e; --ink: #e8eef8; --muted: #94a3b8;
    --line: #26334a; --accent: #34d399; --warn: #fbbf24; --leak: #f87171;
    --soft: #182338;
  }
  * { box-sizing: border-box; }
  body { margin: 0; background: var(--bg); color: var(--ink);
    font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Arial, sans-serif;
    line-height: 1.65; }
  main { max-width: 64rem; margin: 0 auto; padding: 3rem 1.5rem 5rem; }
  h1, h2 { font-family: ui-serif, Georgia, Cambria, "Times New Roman", serif; }
  h1 { font-size: 2.1rem; line-height: 1.2; margin: 0 0 .8rem; }
  h2 { font-size: 1.45rem; font-weight: 600; margin: 3.2rem 0 1rem; }
  .label { font-size: .75rem; text-transform: uppercase; letter-spacing: .08em; color: var(--muted); }
  .kicker { color: var(--accent); margin: 0 0 .6rem; }
  .meta { color: var(--muted); font-size: .9rem; margin: 0; }
  p { margin: .5rem 0; }
  p, li { max-width: 40rem; }
  ul, ol { padding-left: 1.4rem; margin: .5rem 0; }
  li { margin: .3rem 0; }
  li::marker { color: var(--muted); }
  strong { font-weight: 600; }
  code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: .85em; background: var(--soft); border-radius: .25rem; padding: .1em .35em; }
  .card { background: var(--surface); border: 1px solid var(--line);
    border-radius: .5rem; padding: 1.1rem 1.4rem; margin: 1rem 0; }
  .step-head { display: flex; flex-wrap: wrap; align-items: baseline; gap: .6rem; margin-bottom: .35rem; }
  .step-no { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: .875rem; color: var(--muted); }
  h3 { font-size: 1.05rem; font-weight: 600; margin: 0; }
  .files { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: .875rem; color: var(--muted); margin: .2rem 0 .5rem; }
  .badge { display: inline-block; padding: .125rem .5rem; border-radius: 999rem;
    font-size: .75rem; font-weight: 500; }
  .b-new  { background: color-mix(in srgb, var(--accent) 14%, var(--surface)); color: var(--accent); }
  .b-edit { background: color-mix(in srgb, var(--muted) 14%, var(--surface)); color: var(--muted); }
  .b-docs { background: color-mix(in srgb, var(--warn) 14%, var(--surface)); color: var(--warn); }
  .adr { border-left: 3px solid var(--warn); padding: .6rem .95rem; margin: 1rem 0; }
  .adr .label { color: var(--warn); display: block; margin-bottom: .15rem; }
  pre.code { background: linear-gradient(135deg, #303446, #292c3c); color: #c6d0f5;
    border-radius: .5rem; padding: 1.7rem 1.2rem 1rem; overflow-x: auto; position: relative;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: .8rem; line-height: 1.55; margin: .7rem 0; }
  pre.code code { background: none; color: inherit; padding: 0; white-space: pre; }
  pre.code[data-lang]::before { content: attr(data-lang); position: absolute;
    top: .45rem; right: .8rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: .68rem; text-transform: uppercase; letter-spacing: .08em;
    color: rgba(156, 160, 208, .55); }
  .figure { background: #ffffff; border: 1px solid var(--line); border-radius: .5rem;
    padding: 1rem; margin: 1.5rem 0; }
  .figure .label { display: block; margin-bottom: .6rem; }
  .figure svg { max-width: 100%; height: auto; }
  .mermaid { display: flex; justify-content: center; font-size: 1rem; }
  .mermaid .nodeLabel, .mermaid .edgeLabel { font-size: .89rem; }
  .mermaid foreignObject { overflow: visible; }
  .mermaid svg { max-width: none; }
  .scroll { overflow-x: auto; }
  table { border-collapse: collapse; width: 100%; font-size: .92rem; margin: .8rem 0; }
  th { font-size: .75rem; text-transform: uppercase; letter-spacing: .08em; color: var(--muted);
    font-weight: 500; text-align: left; padding: .45rem .6rem .45rem 0;
    border-bottom: 2px solid var(--line); }
  td { border-bottom: 1px solid var(--line); padding: .5rem .6rem .5rem 0; vertical-align: top; }
  td.files { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: .8rem; white-space: normal; }
  footer { margin-top: 3.5rem; padding-top: 1.2rem; border-top: 1px solid var(--line);
    color: var(--muted); font-size: .85rem; }
  footer a { color: var(--ink); }
</style>
</head>
<body>
<main>
  <header>
    <p class="label kicker">repo · #issue (labels)</p>
    <h1>…</h1>
    <p class="meta">…</p>
  </header>
  …
</main>
<script type="module">
  import { createHighlighter } from 'https://cdn.jsdelivr.net/npm/shiki@1.22.2/+esm';
  const highlighter = await createHighlighter({ themes: ['catppuccin-frappe'], langs: ['ruby', 'haml'] });
  const GRAMMAR_FOR = { slim: 'haml' };
  document.querySelectorAll('pre.code code').forEach((code) => {
    const declared = (code.className.match(/language-([\w-]+)/) || [])[1] || 'text';
    const lang = GRAMMAR_FOR[declared] || declared;
    const html = highlighter.codeToHtml(code.textContent, { lang, theme: 'catppuccin-frappe' });
    code.innerHTML = html.replace(/^<pre[^>]*><code[^>]*>/, '').replace(/<\/code><\/pre>\s*$/, '');
  });
</script>
</body>
</html>
```

### Device vocabulary

Every device encodes information; none is decoration.

| Device | Encodes |
|---|---|
| `.card` (article: number → title → badge row → `.files` → body) | one unit of work with its classification; a bare list never gets a card |
| `.badge` emerald / amber / slate | one 3-state classification of the card — never a fourth color |
| `.files` | exact paths, monospaced |
| `.label` | schematic caption — diagram plates, table headers, callout titles only; never an eyebrow above every heading |
| `.adr` | one ruling, one idea — amber bar, no fill, no radius |
| `pre.code` + `data-lang` | code plate: the label names the language truthfully; `class="language-*"` feeds Shiki (catppuccin-frappe — the plate background is frappe base `#303446`, cohesive in both page themes). No CDN highlighter ships a slim grammar — route it through haml via `GRAMMAR_FOR` |
| `.figure` | a diagram plate (light, neutral) + its legend |
| `.scroll` | anything wider than the grid (tables, diagrams, long paths) |

Color budget: one accent (emerald) plus amber for warnings; red appears only inside diagrams, as leakage; frappe lives on code plates only, diagrams stay neutral (standing ruling). Serif headings, sans body, dark mode automatic via `prefers-color-scheme`.

### One scale source

The base font-size is declared exactly once — `:root { font-size: 18px }` — and every other dimension in the stylesheet is `rem` or `em`: type scale, paddings, margins, radii, the measure, the grid width. Changing that single value rescales the whole page proportionally; diagram labels follow through `.mermaid .nodeLabel, .mermaid .edgeLabel { font-size: .89rem }` (= 16px at 18px root). `px` survives only in hairline border widths. The trap this prevents: `rem` tracks the *root*, so changing `body { font-size }` scales only inherited text while paddings and explicit sizes stay put.

### Diagrams

Mermaid on the ELK layout (`@mermaid-js/layout-elk` via `registerLayoutLoaders`, `defaultRenderer: "elk"`), theme `neutral` on the light `.figure` plate — bigger nodes, straight orthogonal arrows; the config in the scaffold (`useMaxWidth: false`, `diagramPadding`, `nodeSpacing`, `rankSpacing`) leaves room and keeps nodes off the plate edges. Mermaid measures HTML edge-label boxes narrower than the rendered 16px text: long edge labels get `<br/>` in the diagram source so the box is measured per shorter line; `foreignObject { overflow: visible }` is the safety net. Mermaid's dagre default renders crooked; beautiful-mermaid (Craft Docs) renders nodes smaller — both tried on a real graph and rejected. Hand-built divs and inline SVG remain an option when the shape is editorial (layers, mass, collapse).

### Formation — what gets the weight

1. **Hero.** Open with the most characteristic thing the subject has — the diagram, the verdict, the one number — not with an introduction paragraph.
2. **Devices encode, never decorate.** Numbered markers only for real sequences (steps, timeline). A "don'ts" list stays a plain list. A callout carries one sentence.
3. **Measure.** Body prose ≤ 80 characters per line (`p, li { max-width: 40rem }` inside the 64rem grid); tables, diagrams and code own the full grid width; the page never scrolls horizontally.
4. **One bold thing.** Spend the boldness on a single element — usually the hero; everything around stays quiet. Before publishing, remove one accessory.
5. **Two passes, then a screenshot.** Plan tokens → review against the anti-defaults below → build → screenshot and self-critique (step 7 of the workflow).
6. **Copy.** Each written element does exactly one job, in the reader's terms, active voice. Header meta is prose or a single locator (`repo · #issue`), not a middot-joined string of attributes.

### Anti-defaults calibration

Revise any of these that show up without a subject-specific reason: middot-joined meta chrome and `WORD — fragment` em-dash labels; ALL-CAPS eyebrows above every heading; content chopped into identical cards when the hierarchy is not identical; a single accented word in a headline; numbered markers on non-sequences; fade/slide entrances (the page is static — no non-user motion); one border-radius and one soft shadow on everything.

### Quality floor

Both themes readable (check each var pair's contrast); responsive down to ~20rem; keyboard focus visible on links; section rhythm lives in exactly one rule (the `h2` top margin) — never cancel it with a `:first-child` reset. Grid and flex children that hold code plates or wide content get `min-width: 0`, or one wide tile stretches the whole row. Inline code wraps (no `white-space: nowrap` on it) — a nowrap `<code>` list once bloated a page to 2400px; short identifiers are unaffected by wrapping.

## Alternatives

- **Claude brand** — only when the user explicitly asks for it: [claude-brand.md](claude-brand.md) (dark `#141413`, warm paper `#FAF9F5`, terracotta `#D97757`, Poppins/Lora from Google Fonts). It is not the default anymore; do not fall back to it out of habit.
- **Complex multi-component artifacts** (state management, routing, shadcn/ui): Anthropic's `web-artifacts-builder` skill (React + Tailwind, bundles to a single HTML) lives in the same submodule at `vendor/anthropic-skills/skills/web-artifacts-builder/` with init/bundle scripts — build and bundle there, then publish the resulting single HTML through this storage.

## Sources

`design.md` and `claude-brand.md` are relative symlinks into `vendor/anthropic-skills` — a git submodule of this repo cloning [github.com/anthropics/skills](https://github.com/anthropics/skills) (Apache-2.0), kept unmodified. A weekly launchd agent (`com.morr.update-artifact-guides`, script `~/dotfiles/bin/update-artifact-guides`, log `~/.local/state/update-artifact-guides.log`) bumps the submodule and commits the moved pointer automatically; to refresh by hand use `git submodule update --remote vendor/anthropic-skills` plus a commit, and after a fresh clone of this repo run `git submodule update --init`. If a symlink breaks (submodule not initialized, moved, or re-cloned), re-link it rather than copying content over. The identity scaffold originates from Matt Pocock's `improve-codebase-architecture` skill (local adaptation in `~/.agents/skills/`, repo `github.com/mattpocock/skills`, MIT).
