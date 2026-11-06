# Docs design system

**Part of the site.** The docs use the landing and `/plugins/` system (`../DESIGN.md`): same tokens, nav, faces and theme rule. Moving between them should not feel like a different site. What the docs add is reading furniture (sidebar, toc, tables, callouts), drawn with the same vocabulary. If you cannot name a reason for a new element, do not add it.

## Principles

1. Text is the interface: fix measure, leading, hierarchy first.
2. Accent means something: links, active state, hover. Never decoration.
3. Density over marketing whitespace. No sky, no covers, no demos here.
4. Perf is sacred: zero external requests, static everything.

## Type

- Body and h1/h2: Nunito (`--font-body`), 16px, line-height 1.65, `--content-max: 720px`.
- Mono (`--font-mono`, JetBrains Mono) is the machine voice: nav, h3 (mostly API names), labels, `th`, code.
- h1 Nunito 800 `clamp(2.1rem, 4vw, 2.75rem)`, `-0.03em`, like the plugins title. The paragraph right after it is the lede: 1.1rem, `--ink-2`.
- h2 Nunito 800 1.55rem with a hairline rule above, like the landing's sections. h3 mono 700 ~1rem, like the landing's item labels. Max 3 levels.
- Labels (sidebar groups, toc, eyebrow, pn-dir, th): mono, lowercase, `--ink-3`, no letter-spacing. Uppercase tracked eyebrows are gone with the old system.
- Both faces are self-hosted in `static/fonts/` and preloaded, so the landing, plugins and docs share one cached copy.

## Color

Tokens are copied from `plugins/index.html` (`--paper`, `--ink`, `--ink-2`, `--ink-3`, `--rule`, `--accent`, `--chip-*`, `--soft`, `--nav-sep`, `--pane-*`). `:root` is dark, `[data-theme="light"]` overrides. Keep them in sync with the plugins page when either changes. Docs-only additions: `--tip` (hue 155), `--warn` (hue 65-75), `--selection`, `--backdrop`, `--modal-shadow`.

Theme: identical head script and `localStorage.theme` rule as the other pages. `html` gets no background, `body` paints `--paper`. Never set the background from script.

## Code blocks

The plugin reader's README pane, value for value (8px radius, 0.84rem, line-height 1.6), in both themes.

Highlighting is the reader's own: Zola's highlighter is off, and `docs-highlight.mjs` (run by `build.sh`) passes each block through `highlight()` from `plugins/readme.js`. One grammar set colours both pages, so a snippet looks the same in the docs and in a plugin README. It covers lua, sh/bash, toml, json, yaml and diff, and anything else stays plain ink. A TextMate theme was tried first and never matched: it colours shell flags, operators and members that the reader leaves alone. `zola serve` shows plain code, the same as the token counts. Tokens are the `--t-*` vars, copied from the plugins page:

| ink | comment | keyword | function/command | type | string | const |
|---|---|---|---|---|---|---|
| `#DDE0EA` | `#878DA9` | `#A2B5FA` | `#F7A87D` | `#A9D5E3` | `#ABD695` | `#EDA8C2` |

To colour a new language, add a grammar to `GRAMMARS` in `readme.js`, so READMEs get it too.

Hover `copy` text button, the same one the plugin reader uses. No title bar, lang label, or dots.

## Components

- Radii follow the site: 4px chips and keycaps, 6px sidebar items and buttons, 8px inputs, tables and callouts, 8px code panes, 10px cards, 12px the search modal.
- Nav: the shared 44px bar, `maki / docs` crumb, `Docs` marked `aria-current`. Its border appears only once scrolled.
- Sidebar: on `--paper`, no border. Link text lines up with the nav logo. Active item: accent text on `--chip-bg`.
- Toc: a 1px rail with a 2px accent marker on the active entry.
- Inline code: plugins chip. `a code`: chip stripped, accent + underline.
- Tables: 8px wrapped, mono `th` on `--soft`, row rules only.
- Callouts: `--soft` box, 3px colored left edge, the label `strong` in the same color.
- Docs index: dense list rows (10.5rem title column + desc) in one 10px box, no cards.
- Idiosyncratic details, exactly two: hover `#` anchors, `~N tokens` per page (injected by `build.sh`, hidden in `zola serve`).

## Banned

Gradients, glassmorphism, hover lifts, card grids with icons, starfields and covers (the landing and plugins own those), purple/indigo accents, colored eyebrows, mac-dots terminal mockups, emoji, shadows on the nav (border shift only, the modal and mobile drawer are excepted).

## Keys

`/` or `Ctrl+K` search, `t` theme, like the other pages.

## Perf

- No external requests. Two preloaded fonts (Nunito 39KB, mono 31KB), shared with the landing and plugins pages. The mono italic loads lazily.
- Animations: transform/opacity only, one rAF scroll handler, reduced-motion guard.
- Lazy `search.json`, hover prefetch, MPA view transitions 140ms.
- All CSS/JS inline in `base.html`, one request per page.

## Verifying

`cd site && ./build.sh` (full: search.json + token spans), then serve `_build/`. `zola build` alone wipes `_build/docs`, including `search.json`. Check both themes, 390px and 1600px, and compare against `/plugins/` side by side. Screenshot previews can lie about color; pixel-sample PNGs.
