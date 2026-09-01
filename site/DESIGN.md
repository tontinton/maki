# Landing page design (`site/index.html`)

Kin to the docs, not a clone. `docs/DESIGN.md` is the docs system; its banned list is docs-scoped.

- Shared: the mono face (same `/docs/fonts/` URL, so the cache carries over), the sienna/salmon accent, dark code panes and the `maki` syntax theme, the theme toggle and its `localStorage.theme` key.
- This page alone: a dusk sky behind the hero and closing section (moon, stars, clouds), warm paper and plum ink, 8-10px radii, no eyebrow labels.

## Type

- Nunito (`--font-body`, self-hosted variable latin woff2, 39KB, preloaded) carries wordmark, tagline, headings, body. Nothing is fetched from Google.
- Mono is the machine voice only: nav breadcrumb, install command, labels, code. Mono body copy is on every AI-slop checklist.
- The wordmark is plain `maki` in Nunito 800: no caret, no accent block, no dot on the `i`. Metrics are load-bearing (`clamp(2.5rem, 5vw, 3.5rem)`, `-0.03em`, `line-height: 1.05`); 4rem reads heavy with the same face. Mono (squat `m`) and Fraunces were both tried and rejected.

## Theme

- A stored choice wins, otherwise `prefers-color-scheme`. The inline head script writes `data-theme` before the first paint, so nothing flips after load. The docs run the identical rule on the same key, down to how an unknown value is treated, or moving between them switches theme.
- `:root` holds the dark tokens with `[data-theme="light"]` overriding, so a JS-less visitor gets dark. `theme-color` is two metas, one per scheme.
- A system change repaints only when nothing is stored, and must not write to storage. Never set the background from script.
- The canvas background lives on `html` and must equal the hero gradient's first stop (`--sky-1`); `.bottom-sky` returns to `--sky-1` at 100% so the bottom overscroll matches. Fixed elements do not paint there, so there is one colour to get right.

## Behaviour

- Keyboard-first like the app: `?` lists keys, `j`/`k` walk sections, `g`/`G` jump, `t` switches theme, `c` copies the install command, typing `doom` finds the DOOM demo. The hero moon is also the theme switch.
- Nothing hijacks text selection. No reading-progress bar: a line filling left to right under a fixed header was reported as a broken scrollbar twice, and the breadcrumb already says where you are.
- Transitions are ~0.1s for hover and colour, 0.13-0.35s for entrances. Ambient sky motion is exempt (cloud drift 38s/52s, twinkle 6s, caret blink 1.15s); speeding it up reads as frantic.

## Install command

Not a widget: no border, no segmented copy button, no OS pill tabs. A shell line at hero scale with the prompt in accent, and the whole line is the copy button.

The fill hugs the command rather than the column: the box is `width: min-content`, so it is as wide as the command's longest unbreakable run, with even padding on both sides. The type is sized from the column instead (`min(1.02rem, (100cqw - 1.8rem) / run)`, with `.hero-intro` as the query container and `run` the character count times the 0.6em mono advance), so that run always fits and the command breaks in one known place or not at all: after `-fsSL` for curl, never for the shorter powershell line. Two dead ends here. A full-column panel leaves the short first line in a box of dead space, and `width: fit-content` does not fix it because the `nowrap` url makes max-content the whole one-line command. Moving the fill onto the line itself with `box-decoration-break: clone` removes the dead space but steps the two fragments against each other, which reads as a rendering bug.

The note under it is only the platform link. The `c` shortcut lives in the keys modal rather than under the command, and a copy answers with a transient `copied` after the link.

Nothing sits beside the command, so it wraps instead of cropping; the old flex row clipped its own copy button whenever the `nowrap` command outgrew the hero column. The platform link is a question (`on Windows?`), because a bare `Windows` under a macOS command reads as a status rather than an action.

## Demos

- No borders. The dark body is the edge; lift with `--demo-shadow` instead. Light needs two layers (tight contact plus wide ambient) or the shadow vanishes against the pale sky; dark needs one near-black layer.
- Both share one transport: same `.player-controls` markup and CSS, play/pause icons swap on a `playing` class rather than from script, so the cast player and DOOM video cannot drift apart.
- Watch for clipping ancestors. The hero shadow sits on `.tui-scale-wrap`, not the `.tui` inside it, because the wrap is `overflow: hidden`. For the same reason `.doom-demo` must not carry `content-visibility: auto`, whose paint containment crops the shadow flush to the frame.
- Sized by content, not by the column, and fitted with a font size (`--cast-font`: 18px times the column ratio) rather than `transform: scale()`. Scaled text is rasterized at one size and resampled to another, which is where the terminal picked up a soft edge; at a real font size every glyph is rendered at the size it is shown at. The pre-boot floor (`min-width: 774px`) stays scoped to the hero's own `.tui:not(.booted)`, or it floors the DOOM frame too and pushes it off small screens. The cast is 86x22 and lands at 856px on desktop (`.hero-grid` `max-width: 1280px`, `30% 1fr`, 2.5rem gap, `.hero` padding `clamp(2rem, 4vw, 4rem)`). The `30%` matters below 1280px: a fixed `24rem` keeps the text column wide and starves the demo. DOOM stops at 900px because the game renders into a 200x124 cell grid.

## The two item sections

`Where tokens go` and `What you get` share one component: a two-column grid of items, each a mono label over one or two short paragraphs (`.features` / `.feature`). Labels that point somewhere carry an arrow, down for this page and up-right for the docs. The label needs `0.6rem` under it, a clear step above the `0.5rem` between prose groups; at `0.3rem` it landed on the same step and the bold mono read as glued to its first line. Neither section has a footer; the 2x claim is the hero's job.

- Every token entry names a mechanism and a number in the same breath ("Costs 59 tok/turn, saves 224 on reads"), and the numbers stay in the sentences. A right-hand column of figures cannot be set: align on a shared right edge and the units run ragged, align the value flush right and the figures stop lining up, stack the unit under the figure and it becomes a dim 0.78rem caption.
- Two earlier forms are dead. A two-column table of keys and notes: a `max-width` on the note cell shortens its `border-top` with it, so every row rule stopped 130px short and the block read as shoved left although it was centred to the pixel; even fixed, a table of prose beside a grid of prose is two components doing one job. Before that, two `<h2>`s side by side with ten equal paragraphs and coloured pips that encoded nothing.
- No `#` anchors beside headings. A hover-only mark in the margin is chrome for a problem six sections do not have.

## Voice

The copy is Tony's: first person, telegraphic, blunt ("so this one is big", "Most agents only see `git`"). When a section is restructured the sentences get cut, not rewritten. A pass that smoothed them came back with em-dash pivots, negative parallelism ("cheaper is not X, it is Y") and evenly weighted triplets, which is what people mean by AI-written. Colons and periods do that work.

Prose runs in groups of one or two sentences with 0.5rem between them, tighter than a paragraph break and looser than a line break, so a block still reads as one item. Four sentences run together is a wall at this measure.

## Order

Claim then proof: the token items run straight into `index` and `code_execution`, the two sections they link to. `What you get` follows, opening on Lua plugins rather than burying the name in the closing list, since the plugin API is the thing worth showcasing; that item links into the Lua section directly below and stops one sentence short of it. The long tail of features is one closing sentence in the body face, never a run of dim mono chips.

## Layout

- The content band is 1140px of inner width, centred. `.content` is `max-width: 1332px` with `padding: 0 clamp(1.25rem, 7vw, 6rem)`, so the gutter stays a percentage until the cap takes over near 1330px. A fixed 3rem pad collapsed it to 4% on a 1250px window and the band read flush left. 1140 is a floor, not taste: the `/standup` Lua sample needs 539px and two panes plus the gap need the rest.
- The hero keeps its own 1280px grid and 856px cast, the one full-bleed element.
- Both item sections are two columns so the text-only sections still fill the band; one column below 1000px.
- One text axis: every heading and body block starts on the same left rule. Centring the provider pills, and then the whole providers section, were both tried and rejected. Only figures and their captions are exceptions: the DOOM demo (media that cannot fill its column) and the two `.stat` lines under the panes they describe.
- Provider chips are a wrapping flex row of content-width pills. An `auto-fit, minmax(190px, 1fr)` grid gives every chip the widest one's column, so `xAI` sits in dead space; the ragged last row is what a tag row looks like. No logos: only 9 of the 17 providers have a simple-icons mark, so the small ones, which are the point, would look unfinished.
- Hairlines are borders, never a 1px box with a background. A background box rounds its two edges to device pixels independently, so on a fractional-DPR display one nav separator lands 2px wide and its neighbour 1px; a border always rasterizes to exactly one device pixel.
- Code panes drop to one column at 1220px, not 1000px, so the squeeze never clips a line. The two Lua samples stack at 1340px because they need 1098px of band. Scrollbars are thinned globally via `--scroll-thumb`.

## Verifying

Serve `site/` (`python3 -m http.server`) and open `/index.html`. Check both themes at 390px and 1600px. Screenshot previews can lie about colour; pixel-sample PNGs.

Fonts live in `docs/static/fonts/` and are referenced as `/docs/fonts/...`, a path Zola only creates when it builds the docs. `site/docs/fonts` is a symlink to `static/fonts` so that URL also resolves when `site/` is served raw. Without it the page silently falls back to system fonts, which looks like the wrong typeface rather than a missing file.
