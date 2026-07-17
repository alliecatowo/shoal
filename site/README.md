# Shoal documentation site

The site is a standalone [Zola](https://www.getzola.org/) project. It has no theme, JavaScript
framework, package manager, or runtime build dependency beyond Zola itself.

## Work locally

Use the same Zola release as Pages:

```bash
zola --version                    # CI uses 0.22.1
zola --root site serve            # http://127.0.0.1:1111
zola --root site check
```

The configured `base_url` is the GitHub Pages project URL. `zola serve` replaces it while serving;
the Pages workflow also passes the URL returned by GitHub's Pages configuration step explicitly.
This keeps links correct in local preview, forks, custom domains, and `/shoal/` project hosting.

## Content model

- `content/docs/` is the public, task-first user manual.
- `content/internals/` is the code atlas and maintainer handbook.
- Section navigation, breadcrumbs, local outlines, adjacent-page links, and search entries are
  generated from front matter rather than duplicated by hand.
- Mermaid diagrams use ordinary fenced `mermaid` blocks. JavaScript replaces them with a compact
  pan/zoom viewer before loading Mermaid; without JavaScript the original fence remains as the
  progressive fallback. Renderer failures show a retry control without exposing source inline.
- `syntaxes/` contains the build-time TextMate grammar used by Zola/Giallo for native `shoal` and
  `shl` fences. Missing fence languages are build errors, so examples cannot silently lose
  highlighting when the documentation vocabulary changes.

Use `shoal` for Shoal source fences and `bash` for host-shell commands. Both are highlighted at
build time; `shl` remains a grammar alias for compatibility with external Markdown.

Useful page metadata:

```toml
+++
title = "Page title"
description = "A concrete one-sentence promise."
weight = 20

[extra]
eyebrow = "Language reference"
group = "Language"
status = "implemented"
audience = "Shell users"
wide = false
+++
```

`group` places a chapter into a named book; `weight` orders chapters across and within books. The
user manual renders `Start here`, `Language`, `Shell & tools`, `Agents & protocol`, `Project`,
and `Reference` in that order. The internal atlas renders `Orientation`, `Language & runtime`,
`Execution & security`, `Kernel & agents`, `Storage & tooling`, and `Maintenance`. A missing group
lands in a final `Guides` book, so drafts remain discoverable instead of disappearing from the
navigation. The book containing the current page opens automatically in the compact sidebar.

Never put generated output in version control. `site/public/` is ignored and uploaded directly as
a Pages artifact.

## Diagram governance

Run `target/debug/shoal site/scripts/check-diagrams.shl` before building. The Shoal program enforces the
curated 123-diagram inventory, per-page limits (three in the Manual, six in Architecture), and
native Mermaid `accTitle`/`accDescr` metadata. Diagrams must explain a relationship already covered
by adjacent prose; they are not decorative substitutes for the text.
