---
name: artifact-builder
description: Create, update, and publish self-contained HTML artifacts to the self-hosted artifacts storage via the `artifacts` MCP server. Use whenever the user asks for an artifact, a published page/dashboard/report/visualization with a shareable link, or wants to update, roll back, or delete an existing artifact.
---

# Artifact Builder

Turn a request into a published artifact with a public URL. The storage is the self-hosted artifacts server (repo `~/develop/work/artifacts`); every read and write goes through the `artifacts` MCP tools (`mcp__artifacts__*`). Never write artifact files into repos, dotfiles, or `/tmp` — the artifact lives in the storage, and its only local representation is the tool call.

## Workflow

1. **Design before code.** The default look is Anthropic's brand ([claude-brand.md](claude-brand.md)) — every artifact ships in it unless the user explicitly asks for a different one. On top of that fixed identity, read [design.md](design.md) and follow its two-pass process: draft a compact plan (type roles, a one-sentence layout concept), review it against the brief for generic "AI-slop" defaults, revise, and only then write code.
2. **Pick a name.** An English kebab-case slug matching `^[a-z0-9][a-z0-9-]{0,63}$`, descriptive of the content (`rails-cache-report`, not `page-1`). If a collision is plausible, check with `list_artifacts` first.
3. **Build one self-contained HTML document.** Inline all CSS/JS; no local file references — the server serves a single file. External CDN references are allowed, but the page must stay usable if they fail to load. Body limit is 10 MB. User-facing text is Russian (`<html lang="ru">`, `<title>` set); identifiers and code comments are English.
4. **Publish.** `save_artifact` for a new name (fails if the name already exists), `update_artifact` for an existing one (fails if missing; archives the previous version). Omit `contentType` for HTML — `text/html; charset=utf-8` is the default.
5. **Verify the page, not the upload.** A tool response that returns the public URL (`<base>/a/<name>`, no auth needed to view) is the success signal — the artifact is stored and served. Never re-check that fact afterwards: no `curl`/HEAD probes of the URL, no `get_artifact`/`list_versions` round-trips "to be sure" — a failed call returns an error, not a URL. What does need checking is the page itself. Before publishing, cross-check markup against the stylesheet: every class used in the body must have a matching rule in the `<style>` block, and repeated devices (section-number spans, badges, tags) are styled by their own class (`.n { … }`), never scoped to a single parent (`h2 .n`) — a context-scoped style silently drops wherever the device reappears under another heading level; if numbering must differ per level, use CSS counters on the heading itself instead of hand-placed spans. For non-trivial layouts, take a quick browser screenshot of the published URL — that is visual QA of the design (overlaps, clipped text, broken SVG), not a check that the upload happened.
6. **Iterate on feedback.** Edit, then `update_artifact` again. To restore a previous state: `list_versions`, then `rollback_artifact` with the version id — the rollback is itself archived, nothing is lost. `delete_artifact` is irreversible: confirm with the user first, every time.

## MCP tools (server `artifacts`)

| Tool | Arguments | Notes |
| --- | --- | --- |
| `list_artifacts` | `prefix?` | newest info per artifact; prefix filters by name |
| `get_artifact` | `name` | current content + meta + public URL; text types come back as utf-8, binary as base64 |
| `save_artifact` | `name`, `content`, `contentType?` | create-only |
| `update_artifact` | `name`, `content`, `contentType?` | update-only; omit `contentType` to keep the current one |
| `list_versions` | `name` | metadata + archived version history |
| `rollback_artifact` | `name`, `version` | version id from `list_versions`, e.g. `2026-10-09T18-22-33-123Z` |
| `rename_artifact` | `name`, `newName` | changes the name and the public URL; content, history and timestamps are preserved, the old URL answers with a redirect |
| `delete_artifact` | `name` | irreversible — ask the user first |

## Design references

The user's standing choice: **artifacts are always in the Claude/Anthropic brand style, no explicit request needed**. A different look is the exception, asked for explicitly.

- [claude-brand.md](claude-brand.md) — symlink to Anthropic's `brand-guidelines`. The **default visual identity** for every artifact: dark `#141413` (text, dark surfaces), light `#FAF9F5` (warm paper background — never pure white), mid gray `#7b786b` — use for `var(--mid)` (meta/footer text); this is a local override, claude-brand.md still names `#b0aea5`, this choice wins. Light gray `#E8E6DC` (subtle surfaces); accents `#D97757` (primary terracotta), `#6A9BCC` (blue), `#788C5D` (green) used sparingly. Type: Poppins for headings, Lora for body — load from Google Fonts CDN, falling back to Arial/Georgia; the page must stay usable when the CDN is unreachable.
- [design.md](design.md) — symlink to Anthropic's `frontend-design`. The **craft authority**: composition, type scale, line lengths, structural devices, motion restraint, self-critique, copywriting. Apply it on top of the brand identity. Note: design.md names the cream-plus-terracotta palette as an "AI-slop tell" for generic output — that warning is about defaults nobody chose; here the palette is the user's explicit standing choice, so it wins, and design.md governs everything else.
- Report layout: the standard content width for a report-style artifact is a centered `max-width: 54rem` page container. A table too wide for it breaks out to `72rem`: `table.wide { width: min(72rem, calc(100vw - 3rem)); margin-left: calc((100% - min(72rem, calc(100vw - 3rem))) / 2); }` — degrades to full container width on narrow screens, centered breakout on wide ones.
- Complex multi-component artifacts (state management, routing, shadcn/ui): Anthropic's `web-artifacts-builder` skill (React + Tailwind, bundles to a single HTML) lives in the same submodule at `vendor/anthropic-skills/skills/web-artifacts-builder/` with init/bundle scripts — build and bundle there, then publish the resulting single HTML through this storage, styled per claude-brand.md.

## Sources

`design.md` and `claude-brand.md` are relative symlinks into `vendor/anthropic-skills` — a git submodule of this repo cloning [github.com/anthropics/skills](https://github.com/anthropics/skills) (Apache-2.0), kept unmodified. A weekly launchd agent (`com.morr.update-artifact-guides`, script `~/dotfiles/bin/update-artifact-guides`, log `~/.local/state/update-artifact-guides.log`) bumps the submodule and commits the moved pointer automatically; to refresh by hand use `git submodule update --remote vendor/anthropic-skills` plus a commit, and after a fresh clone of this repo run `git submodule update --init`. If a symlink breaks (submodule not initialized, moved, or re-cloned), re-link it rather than copying content over.
