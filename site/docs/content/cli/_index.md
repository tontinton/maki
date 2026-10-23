+++
title = "CLI"
weight = 11
[extra]
group = "Reference"
+++

# CLI

`maki` without a subcommand starts the TUI. Subcommands cover auth, models, MCP OAuth, updates, and a few debug helpers. Many flags only apply to one of three run paths: **TUI**, one-shot **`--print`**, or **SDK** (`--print --input-format stream-json`).

```bash
maki [OPTIONS] [PROMPT]
maki <COMMAND>
```

If you pass a prompt (or pipe stdin) without `--print`, the TUI still opens and that text is the first message. With `--print`, Maki runs non-interactively and exits when done.

## Flags by run path

| Flag | TUI | `--print` | SDK (`stream-json`) |
|------|-----|-----------|---------------------|
| `-m` / `--model` | yes | yes | yes |
| `--yolo` | yes | yes | yes (or `--permission-mode bypassPermissions`) |
| `--no-plugins` / `--no-commands` / `--no-jit` | yes | yes | yes |
| `--allowed-tools` / `--disallowed-tools` | yes | yes | yes |
| `-c` / `--continue`, `-r` / `--resume` | yes | yes | yes |
| `--exit-on-done` | yes | n/a (always exits) | n/a |
| `--image` | no (use Ctrl+V paste) | yes | via wire protocol |
| `--verbose`, `--output-format` | no | yes | stream only |
| `--system-prompt`, `--append-system-prompt` | no | no | yes |
| `--session-id`, `--fork-session` | yes | yes | yes |
| `--max-turns` | no | no | yes |
| `--permission-mode` | no | no | yes |
| `--include-partial-messages` | no | no | yes |

### Shared flags (detail)

| Flag | Description |
|------|-------------|
| `-p`, `--print` | Non-interactive run. See [Headless Mode](/docs/headless/) |
| `--image <PATH>` | Attach an image in `--print` mode (repeatable). Paths must be png, jpeg, gif, or webp |
| `-m`, `--model <SPEC>` | Model as `provider/model-id`. Fallback: last used (skipped with a warning when its provider is gone or has no key) → `provider.default_model` in config → auto-detect from available providers |
| `--verbose` | Full turn-by-turn messages in `--print` output |
| `-c`, `--continue` | Resume the most recent session in this directory |
| `-r`, `--resume <ID>` | Resume a specific session (aliases: `-s`, `--session`) |
| `--output-format <text\|json\|stream-json>` | Output shape for `--print` (default `text`) |
| `--input-format <text\|stream-json>` | With `--print`, `stream-json` enters SDK mode |
| `--no-commands` | Skip custom commands from `.maki/commands`, `.claude/commands`, etc. |
| `--no-plugins` | Skip user `init.lua` (global and project); keep the Lua host and builtin plugins so tools and the default keymap still load |
| `--no-jit` | Run plugin Lua on the interpreter with full debug info |
| `--yolo` | Skip permission prompts on gated tools (alias: `--dangerously-skip-permissions`). Deny rules still apply |
| `--trust` | Load the project's `.maki` config for this run without asking, recording no decision. See [Folder Trust](/docs/folder-trust/#containers-and-ci) |
| `--exit-on-done` | Exit when the agent finishes (TUI automation wrappers) |
| `--allowed-tools <LIST>` | Comma-separated allow list (PascalCase or snake_case) |
| `--disallowed-tools <LIST>` | Comma-separated deny list |
| `--session-id <ID>` | Write this run under a chosen id. Errors if a session already exists under it, unless `-r` / `-c` is continuing that same session |
| `--fork-session` | Copy the resumed session's history under a new id, leaving the original untouched. See [Sessions](/docs/headless/#sessions) |
| `--max-turns <N>` | Cap agent turns (SDK) |
| `--system-prompt <TEXT>` | Replace the system prompt (SDK only) |
| `--append-system-prompt <TEXT>` | Append to the built-in system prompt (SDK only) |
| `--permission-mode <MODE>` | SDK: `default`, `acceptEdits`, `plan`, or `bypassPermissions` |
| `--include-partial-messages` | Stream partial deltas in SDK mode |

### Tool name lists

`--allowed-tools` / `--disallowed-tools` accept Claude Code PascalCase (`Read,Edit,Bash`) or snake_case (`read,edit,bash`). Maki lowercases PascalCase to snake_case and checks the result against the built-in tool names, so `CodeExecution` works but `MultiEdit` errors: it normalizes to `multi_edit`, and the tool is called `multiedit`. Write `multiedit` or `edit_lines` as-is. Unknown names error out with the list of valid names. The edit plugin's sub-tools (`multiedit`, `edit_lines`, `insert_lines`) are valid names here even when disabled. Listing a disabled tool has no effect until you enable it in config.

### Permission modes (SDK)

| Mode | Effect |
|------|--------|
| `default` | Normal permission prompts |
| `acceptEdits` | Accepted for Claude Code compatibility; currently same as `default` |
| `plan` | Agent mode plan with plan file `./plan.md` under cwd |
| `bypassPermissions` | Same as `--yolo` for the SDK path |

If both `--yolo` and `--permission-mode` are set, the explicit mode wins. Unknown mode names warn and fall back to `default`.

Several other Claude Code flags are accepted and ignored so existing scripts keep parsing. Maki prints a warning when you pass one of them.

## Subcommands

### `maki auth`

```bash
maki auth login [provider]   # interactive picker if omitted
maki auth logout <provider>
maki auth status
```

`login` stores credentials under the state directory and can write plan / base URL choices into `providers.toml` (see [Configuration](/docs/configuration/#directory-layout) for the platform path). OpenAI and Copilot have dedicated flows; other providers prompt for a key (and a plan when the provider has more than one). Custom providers can be created from the interactive picker.

`status` shows each provider as configured (key on disk), env-only, or missing.

### `maki models`

```bash
maki models
maki models --refresh    # refetch the models.dev catalog
```

One spec per line, warnings on stderr. Built-in and script providers are listed live, catalog-backed providers from the models.dev cache, which expires after 24 hours and supplies model pricing and context windows. `--refresh` refetches it. When the refetch fails, the cached catalog stays in place, the list still prints, and the command exits non-zero.

### `maki session`

```bash
maki session list            # sessions for the current directory
maki session list --global   # sessions from all projects
maki session list --json     # machine-readable JSON array
maki session delete <id>     # asks first, -f skips
maki session export <id>     # JSON transcript to stdout
maki session import [FILE|-] # store a transcript, print the new id
```

Prints stored sessions as a table (id, title, project directory with `$HOME` collapsed to `~`, last update as a relative age), newest first. `--json` prints one JSON array of the same summaries instead, for scripts. A listed id works with `maki -r <id>` to resume it. `delete` removes the session log along with its archives and index entries, and asks for confirmation first unless you pass `-f` / `--force`. Without a terminal to ask on, it refuses outright. A maki that already has the session open will not notice the delete and will lose the rest of that conversation, so close it first. Inside the TUI the same data lives behind `/sessions` (`Ctrl+P`), where `Ctrl+D` deletes.

#### Export and import

`export <SESSION_ID>` prints one JSON document to stdout. It reads the session without claiming it, so it also works for a session whose model this machine cannot run. When another maki has the session open, you get the session as last saved, and a notice on stderr says so.

`import` reads the document from `FILE`, or from stdin when `FILE` is omitted or `-`. It stores the result as a new session in the current directory's project. It prints only the new id on stdout, so `id=$(maki session import < file.json)` works. Continue it with `maki -r <id>`.

On failure, `import` stores nothing and exits non-zero. The error names the place in the document, such as `messages[12].content[2]`. Indexes start at 0, as in the JSON. A document with no messages left after repair is refused, such as one whose only message holds an orphan tool result.

The document has these fields:

| Field | Meaning |
|-------|---------|
| `format` | Version of the document format, currently `1` |
| `model` | Model spec as stored. Import keeps any string and resolves it when the session opens. When it is missing or blank, no model is stored and the session opens on the default model. In the TUI, `maki.session.new` uses the focused tab's model instead |
| `mode` | `build` or `plan` |
| `thinking` | Thinking setting, spelled as in `/thinking` |
| `fast` | Fast mode on or off. Off when missing |
| `title` | Session title. Import keeps a given title and only collapses its whitespace to single spaces. When it is missing or blank, import generates one from the history |
| `facts` | The date, working directory, model, instructions and hints the model was last told. On import the model is told what changed |
| `messages` | The conversation in the same shape [`maki.session.messages`](/docs/lua-api/#maki-session-messages) returns |

Import drops top-level fields it does not know, so keep your own metadata outside the document. A `format` newer than the importer supports fails with "exported by a newer maki". Thinking blocks and their signatures are dropped on import, because they are bound to a system prompt the new session will not have. Orphan tool results and a dangling final tool call are repaired the way a resume repairs them. A content block of a known type that does not parse, or one without a `type`, is refused. A block of a type this maki does not know is kept as it is.

#### Move a session

To continue a session on another machine, export it and import the file there:

```bash
maki session export <id> > session.json
scp session.json server:
ssh -t server 'cd ~/code/my-app && id=$(maki session import ~/session.json) && maki -r "$id"'
```

The import stores the session under the directory you run it in, with a new id. The first run there tells the model the new working directory and date, so it does not keep acting on the old ones. If the `model` in the file is not available on the target machine, the session opens with a warning and falls back like any other resume.

A plugin can do the same without the CLI. `maki.session.export` returns the document as a table, and `maki.session.new` takes it back. This sends the focused session to a server and pulls it back later, with the `upload` and `download` helpers left to you:

```lua
-- hand off from the focused session
local id = maki.session.current()
local doc = assert(maki.session.export({ session = id }))
doc.messages = redact(doc.messages)
local url = assert(upload(maki.json.encode(doc)))
maki.notify("continued at " .. url)

-- pull it back
local back = maki.json.decode(assert(download(url)))
back.focus = true
assert(maki.session.new(back))
```

`export` needs the `session_read` permission and `new` needs `session_write`. Let `redact` edit a copy made with [`maki.deepcopy`](/docs/lua-api/#maki-deepcopy), which keeps `maki.NIL` and empty arrays intact. Without `prompt` or `focus`, `new` only stores the session. Where settings come from and which frontends can open it are in the [`maki.session.new` reference](/docs/lua-api/#maki-session-new).
### `maki mcp`

```bash
maki mcp auth <server>     # OAuth for an HTTP MCP server
maki mcp logout <server>   # drop stored tokens
```

Server names come from your [MCP config](/docs/mcp/). On a machine without a browser, `auth` prints a URL you open elsewhere and paste back.

### `maki update` / `maki rollback`

```bash
maki update            # install latest release
maki update -y         # skip confirmation
maki update --no-color
maki rollback          # previous version
```

Uses the same install locations as the install scripts.

### `maki acp`

```bash
maki acp
maki acp -m anthropic/claude-sonnet-4-6
maki acp --yolo
maki --no-jit acp
```

Starts an [ACP](/docs/acp/) server on stdio for editors like Zed. Subcommand flags are only `-m` / `--model` and `--yolo`. Global flags like `--no-jit` must come before the subcommand.

### `maki index`

```bash
maki index path/to/file.rs
```

Runs the `index` tool on a file and prints the skeleton, so you can see what the agent will get before a session. Builtin plugins always load here; `--no-plugins` only skips user `init.lua`.

### `maki prompt`

```bash
maki prompt                  # rendered system prompt (default: system variant)
maki prompt research
maki prompt general
maki prompt --plan           # system prompt + plan-mode reminder (system only)
maki prompt --tools          # tool definitions as JSON
maki prompt --tools --names  # tool names only, one per line
```

Debug helper for inspecting the prompt and tool surface the agent sees. `--plan` is rejected on non-system variants.

### `maki migrate`

```bash
maki migrate xdg
maki migrate providers
```

`xdg` moves data from `~/.maki/` into platform directories. Safe to re-run. See [Configuration](/docs/configuration/#directory-layout).

`providers` lists the old provider scripts that no Lua plugin replaces yet, and prints a prompt that asks a coding agent to port them. The prompt goes to stdout, so `maki "$(maki migrate providers)"` starts maki on it. See [Migrating from provider scripts](/docs/providers/#migrating-from-provider-scripts).

### `maki trust`

```bash
maki trust add [PATH]
maki trust add [PATH] --yes
maki trust remove [PATH]
maki trust list
```

Records whether a folder's `.maki` configuration may load. `PATH` defaults to
the current directory, `--yes` skips the confirmation, and `list` shows trusted
and rejected folders. See [Folder Trust](/docs/folder-trust/) for what is gated
and for the `--trust` flag that grants trust for a single run.

## Everyday examples

```bash
# TUI on a project
cd ~/code/my-app && maki

# One-shot with YOLO and a model pin
maki -p --yolo -m anthropic/claude-sonnet-4-6 "summarize the architecture"

# Resume yesterday's session
maki --continue

# List models, then log in
maki models
maki auth login

# Inspect tools without starting a session
maki prompt --tools --names
```

For JSON / stream-json output, stdin prompts, and SDK wire mode, see [Headless Mode](/docs/headless/).
