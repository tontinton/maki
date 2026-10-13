use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::UiWaker;
use maki_agent::tools::compact_duration;
use maki_highlight::{DEFAULT_COLOR_NAME, SegmentColor};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::action::run_plugin_action;
use crate::api::keymap::{InputMode, accept_key, switch_mode};
use crate::api::util::command::{
    Anchor, Border, BuiltinAction, Dimension, FloatConfig, HintEntries, HintWriter, InputEdit,
    InputRequest, Split, TextEdit, TitlePos, UiAction, WinCommand, WinEvent, ui_json_roundtrip,
    ui_roundtrip, ui_send,
};
use crate::api::util::convert::{TableExt, reject_unknown_fields, required};
use crate::api::util::pair::{Pair, try_pair};
use crate::docs::{FnDoc, ParamDoc};
use crate::key::Key;
use crate::plugin_permissions::{PluginReach, require};
pub(crate) mod blit;
pub(crate) mod buf;
pub(crate) mod marks;
pub(crate) mod syntax;
pub(crate) mod win;

use crate::runtime::with_task_bufs;
use marks::{InputRule, InputRuleStore, PatternRule, parse_mark};
use syntax::parse_syntax;
use win::{WinHandle, WinSender};

/// `fg`, `bg` and six modifiers.
const UI_STYLE_FIELDS: usize = 8;
const INPUT_EDIT: &str = "input_edit";
const SINGLE_EDIT_KEYS: [&str; 3] = ["start", "stop", "text"];
const NOT_A_LIST_ERR: &str = "is a list of tables, so wrap a single one in {}";
const MIXED_EDIT_SHAPES_ERR: &str =
    "input_edit: pass either 'edits' or 'start'/'stop'/'text', not both";
const EDIT_OPTIONS: &[&str] = &[
    "edits",
    "start",
    "stop",
    "text",
    "version",
    "session_id",
    "cursor",
    "strict",
];
const INPUT_RULES: &str = "input_rules";
const RULE_OPTIONS: &[&str] = &["ns", "pattern", "mark", "treesitter"];
const RULE_HALVES_ERR: &str = "input_rules: 'pattern' and 'mark' go together";
const SYNTAX_ALONE_ERR: &str =
    "input_rules: 'treesitter' takes the place of 'pattern' and 'mark', not both";

/// A focused window is handed every key the host does not answer itself, so a
/// list of keys to take on top of that claims nothing it does not already
/// have. Refused rather than ignored: a plugin writing one has the wrong model
/// of who is reading the keyboard, and that is worth an error it can read.
const FOCUSED_CLAIM_ERR: &str =
    "a focused window already receives every key, so `keys` belongs to `focus = false` windows";

pub(crate) struct HintStore {
    hints: BTreeMap<Arc<str>, Vec<(String, String)>>,
}

impl HintStore {
    pub fn new() -> Self {
        Self {
            hints: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, plugin: Arc<str>, spans: Vec<(String, String)>) {
        if spans.is_empty() {
            self.hints.remove(&plugin);
        } else {
            self.hints.insert(plugin, spans);
        }
    }

    pub fn clear_plugin(&mut self, plugin: &str) {
        self.hints.retain(|k, _| k.as_ref() != plugin);
    }

    pub fn snapshot_entries(&self) -> HintEntries {
        self.hints
            .iter()
            .map(|(k, v)| (Arc::clone(k), v.clone()))
            .collect()
    }
}

/// The windows each plugin has open, so unloading it can take them down.
///
/// A window is the one thing a plugin puts on screen that the host cannot
/// otherwise revoke. The handle lives in the plugin's module table, and a
/// module nobody references any more goes only when the Lua collector next
/// runs. Until then the float is on screen with its key loop cancelled along
/// with the load, and every key it claimed is swallowed on the way to a
/// channel nobody reads: `/reload` would leave the user's `<CR>` doing
/// nothing until a collection they cannot ask for.
#[derive(Default)]
pub(crate) struct WinStore {
    open: BTreeMap<Arc<str>, Vec<WinSender>>,
}

impl WinStore {
    /// Drops the windows that have already gone on the way in, so a plugin
    /// opening and closing one per keystroke does not grow this for the run.
    fn track(&mut self, plugin: Arc<str>, cmd_tx: WinSender) {
        let windows = self.open.entry(plugin).or_default();
        windows.retain(|tx| !tx.is_disconnected());
        windows.push(cmd_tx);
    }

    /// The same command the plugin's own `win:close()` sends, so the window
    /// leaves by the one path every other window leaves by and the plugin's
    /// loop still hears the close it is waiting on.
    pub fn close_plugin(&mut self, plugin: &str) {
        for tx in self.open.remove(plugin).unwrap_or_default() {
            tx.send(WinCommand::Close);
        }
    }
}

fn publish_hint_snapshot(lua: &Lua) {
    if let Some(store) = lua.app_data_ref::<HintStore>() {
        let entries = store.snapshot_entries();
        if let Some(writer) = lua.app_data_ref::<HintWriter>() {
            writer.publish(entries);
        }
    }
}

pub(crate) fn parse_footer(tbl: &Table) -> LuaResult<Vec<(String, String)>> {
    let footer_tbl: Table = match tbl.get("footer") {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    footer_tbl
        .sequence_values::<Table>()
        .map(|entry| {
            let entry = entry?;
            Ok((entry.get(1)?, entry.get(2)?))
        })
        .collect()
}

/// Creates a new buffer for building UI content. The first buffer
/// created in a task becomes the "live" buffer, streamed to the UI while
/// the tool runs, which is what the tool's own output pane wants. A
/// float that opens during a tool call would take that spot away, so
/// create its buffer with `{ scratch = true }`. It matches nvim's
/// `nvim_create_buf(false, true)`. A buffer belongs to the task that
/// made it, so this throws at a plugin's top level.
///
/// @param opts table? Optional. `scratch` (boolean) keeps the buffer out of the live slot, default false.
/// @return (Buf) Buffer handle.
/// @example
/// -- The tool's output pane:
/// local out = maki.ui.buf()
/// out:line("hello world")
///
/// -- A float raised during a tool call needs its own buffer:
/// local toast = maki.ui.buf({ scratch = true })
/// toast:line("copied!")
#[lua_fn]
fn buf(lua: &Lua, opts: Option<Table>) -> LuaResult<buf::BufHandle> {
    let scratch = opts.and_then(|t| t.opt_bool("scratch")).unwrap_or(false);
    with_task_bufs(lua, |store| {
        if scratch {
            store.create()
        } else {
            store.create_live()
        }
    })
}

/// Looks up a color the syntax theme names, such as "background",
/// "foreground" or "accent". For the styles the UI paints with, use
/// `maki.ui.theme_style`.
///
/// @param name string Syntax theme color name, e.g. "accent" or "background".
/// @return (string|nil) "#rrggbb" for a truecolor theme, a palette index as a
///   string like "4" when the theme names an ANSI color, or "default" for the
///   terminal's own color. Nil only when the name is unknown. Every form can be
///   passed straight to a span's `fg`/`bg`.
/// @example
/// local accent = maki.ui.theme_color("accent")
/// if accent then
///   buf:line({ { "note", { fg = accent, bold = true } } })
/// end
#[lua_fn]
fn theme_color(lua: &Lua, name: String) -> LuaResult<mlua::Value> {
    let Some(color) = maki_highlight::theme_color(&name) else {
        return Ok(mlua::Value::Nil);
    };
    Ok(mlua::Value::String(
        lua.create_string(segment_color_to_lua(color))?,
    ))
}

/// Looks up a named style from the current theme. The names are the ones a span
/// already takes as a string ("dim", "item_selected", "keybind_section",
/// "diff_old", ...), so `{ text, "dim" }` and `theme_style("dim")` paint the
/// same. Reach for the table when you need the parts, say to keep a style's
/// foreground over a background of your own.
///
/// @param name string Style name, the same spelling a span accepts.
/// @return (table|nil) `{fg?, bg?, bold?, italic?, underline?, dim?,
///   strikethrough?, reversed?}`, ready to use as a span style. Colors are
///   spelled as in `maki.ui.theme_color`. Nil when the name is unknown, and an
///   empty table when the theme leaves that style unset.
/// @example
/// local sel = maki.ui.theme_style("item_selected")
/// local dim = maki.ui.theme_style("dim")
/// buf:line({ { "note", { fg = dim.fg, bg = sel.bg } } })
#[lua_fn]
fn theme_style(lua: &Lua, name: String) -> LuaResult<mlua::Value> {
    let Some(style) = maki_highlight::ui_style(&name) else {
        return Ok(mlua::Value::Nil);
    };
    let tbl = lua.create_table_with_capacity(0, UI_STYLE_FIELDS)?;
    for (key, color) in [("fg", style.fg), ("bg", style.bg)] {
        if let Some(c) = color {
            tbl.raw_set(key, segment_color_to_lua(c))?;
        }
    }
    for (key, on) in [
        ("bold", style.bold),
        ("italic", style.italic),
        ("underline", style.underline),
        ("dim", style.dim),
        ("strikethrough", style.strikethrough),
        ("reversed", style.reversed),
    ] {
        if on {
            tbl.raw_set(key, true)?;
        }
    }
    Ok(mlua::Value::Table(tbl))
}

/// Syntax-highlights a chunk of source code. Returns a table of styled
/// lines that you can feed into a buffer. Each line is a list of
/// `{text, style}` spans where style is a `{fg, bold?, italic?, underline?}` table.
///
/// `fg` is "#rrggbb" for a truecolor theme, a palette index as a string like
/// "4" when the theme names an ANSI color, or "default" for the terminal's own
/// color. Pass the span straight to `buf:line` and it resolves correctly in
/// every case.
///
/// @param code string Source text to highlight.
/// @param lang string Language identifier, e.g. "rust", "python".
/// @param opts table? Options. Fields:
///   - independent (boolean): highlight each line without cross-line context. Default false.
///   - prefix (string): prepend to the source before highlighting (affects token context). Default "".
/// @return (table) Lines: `{ { {text, style}, ... }, ... }`. Each style is `{fg, bold?, italic?, underline?}`.
/// @example
/// local lines = maki.ui.highlight("fn main() {}", "rust")
/// for _, spans in ipairs(lines) do
///   buf:line(spans)
/// end
#[lua_fn]
async fn highlight(lua: Lua, code: String, lang: String, opts: Option<Table>) -> LuaResult<Table> {
    let independent = opts
        .as_ref()
        .and_then(|t| t.opt_bool("independent"))
        .unwrap_or(false);
    let prefix = opts
        .and_then(|t| t.get::<String>("prefix").ok())
        .unwrap_or_default();
    let segments = smol::unblock(move || {
        maki_highlight::pool::run(move || {
            if independent {
                maki_highlight::highlight_lines_independent(&lang, &code)
            } else {
                maki_highlight::highlight_code(&lang, &code, &prefix)
            }
        })
    })
    .await;
    segments_to_lua_lines(&lua, &segments)
}

/// Renders Markdown into styled lines ready to display in a buffer.
/// Each span's style is either a named string ("bold", "heading",
/// "inline_code", etc.) or a `{fg, bold?, italic?, underline?}` table
/// for syntax-highlighted code blocks.
///
/// @param text string Markdown source.
/// @param width integer Wrap width in columns.
/// @return (table) Lines: `{ { {text, style}, ... }, ... }`.
/// @example
/// local size = maki.ui.terminal_size()
/// local lines = maki.ui.markdown("# Hello\n\nSome **bold** text.", size.cols)
/// for _, spans in ipairs(lines) do
///   buf:line(spans)
/// end
#[lua_fn]
async fn markdown(lua: Lua, text: String, width: u16) -> LuaResult<Table> {
    let lines = smol::unblock(move || {
        maki_highlight::pool::run(move || maki_markdown::render::render(&text, width))
    })
    .await;
    markdown_lines_to_lua(&lua, &lines)
}

/// Formats a number of seconds into a short, human-friendly string.
/// Useful for displaying elapsed time in status messages.
///
/// @param secs integer Duration in seconds.
/// @return (string) Human-readable duration, e.g. "1m30s".
/// @example
/// maki.ui.humantime(90)   -- "1m30s"
/// maki.ui.humantime(3661) -- "1h1m1s"
#[lua_fn]
fn humantime(_lua: &Lua, secs: u64) -> LuaResult<String> {
    Ok(compact_duration(secs))
}

/// Returns the current terminal size. Handy for sizing floating windows
/// or wrapping text to fit the screen.
///
/// @return (table) `{cols, rows}`, terminal width and height in characters.
/// @example
/// local size = maki.ui.terminal_size()
/// local half_width = math.floor(size.cols / 2)
#[lua_fn]
fn terminal_size(lua: &Lua) -> LuaResult<Table> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let tbl = lua.create_table()?;
    tbl.set("cols", cols)?;
    tbl.set("rows", rows)?;
    Ok(tbl)
}

/// Returns the display width of a string in terminal cells, matching
/// how `ratatui` measures text.
///
/// @param text string The text to measure.
/// @return (integer) Number of display cells the text occupies.
/// @example
/// local w = maki.ui.display_width("hello")
#[lua_fn]
fn display_width(_lua: &Lua, text: String) -> LuaResult<usize> {
    use unicode_width::UnicodeWidthStr;
    Ok(text.width())
}

/// Splits a string at a display-cell boundary.
///
/// @param text string The text to split.
/// @param max_width integer Maximum display cells for the head.
/// @return (table) `{head = string, tail = string}`.
/// @example
/// local t = maki.ui.truncate_text("hello world", 5)
/// -- t.head == "hello", t.tail == " world"
#[lua_fn]
fn truncate_text(lua: &Lua, text: String, max_width: usize) -> LuaResult<Table> {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    let mut idx = 0;
    for (i, c) in text.char_indices() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max_width {
            break;
        }
        width += w;
        idx = i + c.len_utf8();
    }
    let tbl = lua.create_table()?;
    tbl.set("head", &text[..idx])?;
    tbl.set("tail", &text[idx..])?;
    Ok(tbl)
}

/// Shows a short-lived message in the status bar, such as "copied!" or a
/// transient warning. Without a UI, the message goes to the log.
///
/// @param msg string Message text.
/// @return
/// @example
/// maki.ui.flash("Copied to clipboard!")
#[lua_fn]
fn flash(
    _lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    msg: String,
) -> LuaResult<()> {
    if ui_send(tx.as_ref(), UiAction::Flash(msg.clone())).is_err() {
        tracing::info!(plugin = %plugin, "{msg}");
    }
    Ok(())
}

/// Sets the terminal emulator's window title. Pass an empty string to
/// clear it.
///
/// The title passes through tmux, GNU screen, and zellij untouched, and
/// control characters are stripped, so model text cannot inject escape
/// sequences into the terminal. On exit maki hands the title back to the
/// shell, on terminals that support the title stack.
///
/// @param title string New window title, e.g. `"● 3/5 tests"`.
/// @return
/// @example
/// maki.ui.set_window_title("maki: " .. session_name)
/// -- Give the title back to the shell:
/// maki.ui.set_window_title("")
#[lua_fn]
fn set_window_title(
    _lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    title: String,
) -> LuaResult<()> {
    let _ = ui_send(tx.as_ref(), UiAction::SetWindowTitle(title));
    Ok(())
}

/// Switches the chat input to normal mode (`"n"`) or insert mode (`"i"`),
/// like leaving and entering insert mode in Neovim.
///
/// The mode is global and stays until a plugin switches it again, apart from
/// the times [Modes](/docs/lua-api/#modes) lists. It applies at once:
/// `maki.fn.mode()` returns it and the next key is matched against it. If
/// your plugin unloads while the input is in normal mode, maki switches it
/// back to insert mode.
///
/// Throws on any mode but `"n"` and `"i"`.
///
/// @param mode string `"n"` or `"i"`.
/// @return
/// @example
/// maki.keymap.set("n", "i", function() maki.ui.set_mode("i") end)
#[lua_fn]
fn set_mode(lua: &Lua, #[ctx] plugin: Arc<str>, mode: String) -> LuaResult<()> {
    switch_mode(lua, InputMode::parse(&mode)?, &plugin);
    Ok(())
}

/// Runs a built-in or plugin action by name, and returns whether it handled
/// the request.
///
/// A built-in does what its default key does, so you can move it to another
/// key:
///
/// | Built-in | What it does | `false` when |
/// | --- | --- | --- |
/// | `"file_picker"` | Opens the file picker | |
/// | `"search"` | Opens message search | |
/// | `"help"` | Opens or closes `/help` | |
/// | `"model_picker"` | Opens the model picker | |
/// | `"plan_toggle"` | Opens or closes the plan form | no plan is ready |
/// | `"plan_editor"` | Opens the plan in your editor | there is no plan |
/// | `"edit_input"` | Opens the chat input in your editor | |
/// | `"pop_queue"` | Removes the first queued message | |
/// | `"prev_chat"`, `"next_chat"` | Shows the previous or next chat, with its own draft | |
/// | `"input_raw"` | Switches the input between drawing what plugins conceal and showing the text under it, which is what gets sent | |
/// | `"undo"`, `"redo"` | Undoes or redoes one input edit | nothing to undo or redo |
/// | `"history_prev"`, `"history_next"` | Recalls the previous or next prompt | at either end of the history |
/// | `"newline"` | Inserts a new line | |
///
/// `"undo"`, `"redo"`, `"history_prev"`, `"history_next"` and `"newline"`
/// edit the chat input, also in normal mode. They need the `input`
/// permission, see [chat input](#chat-input), and throw without it like any
/// gated call. They return `false` in a finished subagent's chat, which has
/// no input. `"pop_queue"` needs `session_write`, because it throws away a
/// queued prompt. The other built-ins need no permission.
///
/// A built-in runs in the UI after your earlier `maki.ui` calls, and
/// `action` waits for its answer. While your plugin loads, it returns `nil`
/// and an error at once.
///
/// A plugin action has a dot in its name, like `"completion.accept"`. It
/// comes from [`maki.api.register_action`](#maki-api-register_action), which
/// also says what permissions a caller needs. It runs at once, also while
/// your plugin loads, and `action` returns what it returned.
///
/// `action` returns `nil` and an error, which an `if` treats as not handled,
/// when:
///
/// - The action's plugin is not loaded.
/// - The action threw.
/// - Your plugin lacks a permission a plugin action needs. The status bar
///   shows this too.
/// - A built-in runs without a UI.
///
/// It throws on a name that cannot be right: no dot and not a built-in, or
/// one its loaded plugin never registered.
///
/// To try several actions on one key, see
/// [Composing keys](/docs/keybindings/#composing-keys). For slash commands,
/// see `maki.api.run_command`.
///
/// @param name string Action name, e.g. `"file_picker"` or `"completion.accept"`.
/// @return (boolean|nil, string|nil) Whether the action handled it, or nil and an error.
/// @example
/// -- A vim plugin's undo in normal mode, which needs `input`:
/// maki.keymap.set("n", "u", function() maki.ui.action("undo") end)
///
/// -- A key for a built-in that has none:
/// maki.keymap.set({ "n", "i" }, "<M-r>", function() maki.ui.action("input_raw") end)
#[lua_fn]
async fn action(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    #[ctx] caller: PluginReach,
    name: String,
) -> LuaResult<Pair<bool>> {
    let Ok(builtin) = name.parse::<BuiltinAction>() else {
        return run_plugin_action(&lua, tx.as_ref(), &name, &plugin, &caller).await;
    };
    for &perm in builtin.needs() {
        require(&lua, caller.permissions(), perm)?;
    }
    let reply = try_pair!(
        ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::Builtin {
            action: builtin,
            caller: caller.now(&lua),
            reply_tx,
        })
        .await
    );
    Ok((Some(reply), None))
}

async fn input_roundtrip(
    lua: Lua,
    tx: Option<&flume::Sender<UiAction>>,
    req: InputRequest,
) -> LuaResult<Pair<mlua::Value>> {
    ui_json_roundtrip(&lua, tx, |reply_tx| UiAction::Input { req, reply_tx }).await
}

/// Reads the chat input text and the cursor position.
///
/// Offsets are byte offsets into `text`, the unit the Lua string library
/// indexes by, so `text:sub(1, cursor)` is everything before the cursor. A
/// newline counts as one byte.
///
/// The returned table has:
///
/// - `session_id` (string) the tab the value was read from. Pass it to
///   `input_edit`, which refuses once another tab is focused.
/// - `text` (string) the whole value, newlines included.
/// - `cursor` (integer) byte offset of the cursor into `text`.
/// - `version` (integer) counter of changes to the value. Pass it to
///   `input_edit`, which moves the edit past what changed since or refuses
///   it.
///
/// The cursor line and column are a slice of those two, so the table leaves
/// them out: with `local before = st.text:sub(1, st.cursor)`,
/// `select(2, before:gsub("\n", ""))` is the 0-based line and
/// `#before:match("[^\n]*$")` the byte column inside it.
///
/// To put a window on the caret, open it with `anchor = "input_caret"`. The
/// host re-places it every frame, so it follows wraps and resizes.
///
/// @return (table|nil, string|nil) The input state, or nil and an error.
/// @example
/// local st = maki.ui.input()
/// local before = st.text:sub(1, st.cursor)
#[lua_fn(guard = SessionRead)]
async fn input(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Pair<mlua::Value>> {
    input_roundtrip(lua, tx.as_ref(), InputRequest::Read).await
}

/// Read as a `Vec`, a table keeps only its list part, so a single item
/// passed in place of the list {key} of {func} would quietly read as none.
fn table_list(list: Table, func: &str, key: &str) -> LuaResult<Vec<Table>> {
    if list.pairs::<mlua::Value, mlua::Value>().count() != list.raw_len() {
        return Err(mlua::Error::runtime(format!(
            "{func}: '{key}' {NOT_A_LIST_ERR}"
        )));
    }
    list.sequence_values().collect()
}

/// Replaces byte ranges of the chat input, as if the user had selected each
/// one and typed its `text`. All of them land together as one change: one
/// new version and one undo step.
///
/// With `edits = {}` and a `cursor`, the call only moves the cursor. The
/// version stays, no undo step is made, and a recalled history entry stays
/// recalled. An empty list with no cursor is refused, since there is nothing
/// to do.
///
/// Every range is planned against the value `maki.ui.input` returned at
/// `version`, so offsets never account for the other edits in the list.
/// Ranges must not overlap, and no two may start at the same offset.
///
/// A handler runs after the key that woke it, so the user may have typed in
/// between. An edit planned against an older version is moved past what
/// changed since, as long as nothing changed at or next to any of its
/// ranges: typing at the far end of the prompt does not refuse a fix at the
/// start, while typing right where the edit lands does. Edits are refused:
///
/// - When a later change overlaps or touches one of the ranges. Two inserts
///   at the same offset count, because neither order is right for both.
/// - When `cursor` lands inside text changed since.
/// - When the value was replaced since: history recall, a draft restored on
///   a tab switch, `$EDITOR`, or a submit.
/// - When the version is so old that the input no longer remembers what
///   changed since then.
/// - When the version is not current and `strict` is set.
/// - When a range ends past the end of the value, or an offset lands inside
///   a multi-byte character.
/// - When `session_id` names a tab that is not focused. Both guards are
///   required and neither substitutes for the other: every tab counts
///   versions from zero.
/// - When the user cannot see the chat input, since text written there would
///   be sent later without ever being read. A permission prompt, the plan
///   form, a pack review, a `below` split, a finished subagent's chat and a
///   terminal too short to give the box a text row all take it off screen,
///   and a picker, a modal or a focused plugin window covers it.
///
/// Read again and retry on any of them.
///
/// A change needs `input`. What Enter does with text you wrote is in
/// [chat input](#chat-input).
///
/// Tabs and carriage returns in `text` become spaces and newlines, and the
/// other control characters are dropped, the way a paste is rewritten. With
/// `cursor` given, such text is refused instead, since the cursor counts
/// bytes that would not land. Clean it first.
///
/// It also needs `session_read`, like `maki.ui.input`. Its answer and its
/// errors tell where the user typed since `version`, so a plugin could
/// learn the input from them.
///
/// @param opts table Options:
///   `edits` (table|nil) list of `{ start, stop, text }` tables, the ranges to replace, `{}` for none. Without it, `start`, `stop` and `text` are read from {opts} itself as a single edit.
///   `start` (integer) byte offset the replaced range starts at.
///   `stop` (integer) byte offset it ends at. `start == stop` inserts.
///   `text` (string) what to put there, `""` to delete the range. Required, so a misspelled key cannot empty it by accident.
///   `version` (integer) the version `maki.ui.input` returned, which the offsets were planned against.
///   `session_id` (string) the session `maki.ui.input` read the offsets from.
///   `cursor` (integer|nil) byte offset to leave the cursor at, counted in the value the edits produce, as if no one else had changed it. Default is the end of the text of the edit with the highest `start`, wherever it sits in `edits`.
///   `strict` (boolean|nil) refuse any version but the current one instead of moving the edits forward. Default false.
/// @return (table|nil, string|nil) `{ version, cursor }` of the input after the edit, or nil and an error.
/// @example
/// local st = maki.ui.input()
/// -- Replace the "@src/ma" before the cursor with a full path:
/// maki.ui.input_edit({
///   start = 8,
///   stop = st.cursor,
///   text = "src/main.rs",
///   version = st.version,
///   session_id = st.session_id,
/// })
///
/// -- Wrap bytes s..e in bold, leaving the cursor before the closing "**":
/// local r, err = maki.ui.input_edit({
///   edits = {
///     { start = s, stop = s, text = "**" },
///     { start = e, stop = e, text = "**" },
///   },
///   cursor = e + 2,
///   version = st.version,
///   session_id = st.session_id,
/// })
#[lua_fn(guard = [Input, SessionRead])]
async fn input_edit(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] caller: PluginReach,
    opts: Table,
) -> LuaResult<Pair<mlua::Value>> {
    reject_unknown_fields(&opts, EDIT_OPTIONS, INPUT_EDIT)?;
    let edits = match opts
        .opt::<Table>("edits")?
        .map(|edits| table_list(edits, INPUT_EDIT, "edits"))
        .transpose()?
    {
        None => vec![text_edit(&opts)?],
        Some(_)
            if SINGLE_EDIT_KEYS
                .iter()
                .any(|key| opts.contains_key(*key).unwrap_or(false)) =>
        {
            return Err(mlua::Error::runtime(MIXED_EDIT_SHAPES_ERR));
        }
        Some(edits) => edits
            .iter()
            .map(|edit| {
                reject_unknown_fields(edit, &SINGLE_EDIT_KEYS, INPUT_EDIT)?;
                text_edit(edit)
            })
            .collect::<LuaResult<_>>()?,
    };
    let req = InputRequest::Edit(InputEdit {
        edits,
        cursor: opts.opt("cursor")?,
        version: required(&opts, INPUT_EDIT, "version")?,
        strict: opts.opt_bool("strict").unwrap_or_default(),
        session_id: required(&opts, INPUT_EDIT, "session_id")?,
        caller: caller.now(&lua),
    });
    input_roundtrip(lua, tx.as_ref(), req).await
}

/// Decorates the chat input in every session, from a regex or from a
/// tree-sitter query. No Lua runs per keystroke.
///
/// Each `ns` of a plugin holds one rule. Calling again with the same `ns`
/// replaces it, and leaving out `pattern`, `mark` and `treesitter` removes
/// it. Rules go away when the plugin unloads.
///
/// A regex rule marks every match with `mark`. The UI matches a line the
/// first time it needs it and keeps the result until the line changes, so
/// the marks never lag behind the text. The pattern uses
/// [Rust regex syntax](https://docs.rs/regex/latest/regex/#syntax). It is
/// matched one line at a time, so a match never spans lines. It still sees
/// the text before its line: `\A` is the start of the input,
/// which only the first line can match, while `$` is the end of the line
/// and `(?m)^` the start of one. An empty match marks a point, which only
/// `virt_text` shows. In `conceal` and the text of `virt_text`, `$1` is the
/// text of capture group 1, `$0` the whole match, and `$$` a plain `$`.
///
/// A tree-sitter rule parses the input as `lang` and runs a query per
/// language. Each capture is marked with the template of the same name in
/// `captures`, and every capture needs one, except `@injection.content`,
/// whose template is optional, `@injection.language`, which names the
/// language of injected code, and names starting with `_`, which only feed
/// predicates. Templates are drawn as written, `$` included. Conditions are
/// the predicates of `maki.treesitter.query`, and `#set!` only takes
/// `injection.language`. Code captured as `@injection.content` is in the
/// language `@injection.language` caught, or the one
/// `#set! injection.language` names. When the rule has a query for that
/// language, the code is parsed again with it and that query runs there.
/// Otherwise it gets a `lang` mark. Markdown leaves the text of its blocks
/// to `markdown_inline`, so a `markdown` rule runs its `markdown_inline`
/// query there by itself.
///
/// The parse runs on a worker thread and never holds up typing. Until the
/// worker catches up with an edit, its marks move with the text, so a bold
/// or a code block grows with what is typed inside it. Only a conceal or an
/// `atomic` mark the edit lands inside is gone until then.
///
/// A rule whose `ns` was first set later is drawn over one set earlier, and
/// replacing a rule keeps its place. Within a tree-sitter rule a later
/// pattern is drawn over an earlier one, and `markdown_inline` over
/// `markdown`.
///
/// maki draws the `!` of a shell command and its bash colors itself, under
/// every rule.
///
/// A mark never edits the text, so a send carries the raw text, concealed
/// ranges included.
///
/// @param opts table Options:
///   `ns` (string) the name of the rule, unique per plugin. Code in the global and the project `init.lua` runs as plugins `"global/init.lua"` and `"project/init.lua"`.
///   `pattern` (string|nil) the regex to match.
///   `mark` (table|nil) what each match gets, with the fields below.
///   `mark.hl` (string|table|nil) a theme style name like `"accent"`, or an inline style `{ fg, bg, bold, italic, underline, dim, strikethrough, reversed }`.
///   `mark.conceal` (string|nil) text drawn instead of the match, `""` hides it.
///   `mark.reveal` (string|nil) `"cursor"` shows the raw text while the cursor is inside the match or on its edge.
///   `mark.atomic` (boolean|nil) the cursor steps over the match, and a delete that cuts into it removes all of it.
///   `mark.virt_text` (table|nil) spans drawn but never part of the text, `{ { text, hl? }, ... }`.
///   `mark.virt_text_pos` (string|nil) `"eol"` (default) after the line, or `"inline"` before the match.
///   `mark.lang` (string|nil) highlight the match as code in this language.
///   `treesitter` (table|nil) a tree-sitter rule instead of `pattern` and `mark`.
///   `treesitter.lang` (string) the language the input is parsed as, like `"markdown"`.
///   `treesitter.query` (string|table) the query for `lang`, or a table of language = query.
///   `treesitter.captures` (table|nil) capture name = template, with the fields of `mark`.
/// @return
/// @example
/// -- Draw `$skill:go` as a ` go ` chip the cursor steps over:
/// maki.ui.input_rules({
///   ns = "skill",
///   pattern = [[\$skill:([\w-]+)]],
///   mark = { conceal = " $1 ", hl = "accent", atomic = true },
/// })
/// -- And remove it again:
/// maki.ui.input_rules({ ns = "skill" })
/// -- Bold text without its stars, unless the cursor is on it:
/// maki.ui.input_rules({
///   ns = "md",
///   treesitter = {
///     lang = "markdown",
///     query = { markdown_inline = [[ (strong_emphasis (emphasis_delimiter) @hidden) @strong ]] },
///     captures = {
///       hidden = { conceal = "", reveal = "cursor" },
///       strong = { hl = { bold = true } },
///     },
///   },
/// })
#[lua_fn]
fn input_rules(lua: &Lua, #[ctx] plugin: Arc<str>, opts: Table) -> LuaResult<()> {
    let fail = |msg: String| mlua::Error::runtime(format!("{INPUT_RULES}: {msg}"));
    reject_unknown_fields(&opts, RULE_OPTIONS, INPUT_RULES)?;
    let ns: String = required(&opts, INPUT_RULES, "ns")?;
    let rule = match (
        opts.opt::<String>("pattern")?,
        opts.opt::<Table>("mark")?,
        opts.opt::<Table>("treesitter")?,
    ) {
        (None, None, None) => None,
        (Some(pattern), Some(mark), None) => Some(InputRule::Pattern(
            PatternRule::new(&pattern, parse_mark(&mark)?).map_err(fail)?,
        )),
        (None, None, Some(syntax)) => Some(InputRule::Syntax(Arc::new(
            parse_syntax(&syntax)?.map_err(fail)?,
        ))),
        (_, _, Some(_)) => return Err(mlua::Error::runtime(SYNTAX_ALONE_ERR)),
        _ => return Err(mlua::Error::runtime(RULE_HALVES_ERR)),
    };
    if let Some(mut store) = lua.app_data_mut::<InputRuleStore>() {
        store.set(plugin, ns, rule);
    }
    Ok(())
}

fn text_edit(edit: &Table) -> LuaResult<TextEdit> {
    Ok(TextEdit {
        start: required(edit, INPUT_EDIT, "start")?,
        stop: required(edit, INPUT_EDIT, "stop")?,
        text: required(edit, INPUT_EDIT, "text")?,
    })
}

/// Opens {path} in the user's `$EDITOR` (e.g. vim, nano) and suspends the
/// TUI until the editor exits. An exit code of 0 does not mean the user
/// saved: read the file back to see what changed.
///
/// @param path string File to open.
/// @return (integer) Editor exit code, or -1 if the editor failed to start or there is no UI.
/// @example
/// local code = maki.ui.open_editor("/tmp/scratch.lua")
/// if code ~= 0 then
///   maki.ui.flash("editor exited with " .. code)
/// end
#[lua_fn]
async fn open_editor(
    _lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    path: String,
) -> LuaResult<i32> {
    let path = PathBuf::from(path);
    Ok(ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::OpenEditor {
        path,
        reply_tx,
    })
    .await
    .unwrap_or(-1))
}

/// The keys an unfocused window takes while it is on screen, read through the
/// same gate `maki.keymap.set` reads, so the two can never drift.
///
/// Every key is parsed before the window is opened, so a typo leaves the
/// plugin with no window rather than a window holding half a list.
fn parse_claimed_keys(opts: &Table, focus: bool) -> LuaResult<Vec<Key>> {
    let Some(keys) = opts.opt::<Table>("keys")? else {
        return Ok(Vec::new());
    };
    if focus {
        return Err(mlua::Error::runtime(FOCUSED_CLAIM_ERR));
    }
    keys.sequence_values::<String>()
        .map(|lhs| accept_key(&lhs?))
        .collect()
}

/// Opens a floating or split window that displays the contents of {buf}.
/// Returns a Win handle you can use to receive events, update layout,
/// and close the window when you are done.
///
/// @param buf Buf Buffer to display.
/// @param opts table Float configuration. Fields:
///   - width (integer|string): window width. Integer for absolute columns; "N%" for percent of terminal width. Default "60%".
///   - height (integer|string): window height. Integer for absolute rows; "N%" for percent of terminal height. Default "70%".
///   - row (integer?): row offset from the anchor corner. Negative values move up.
///   - col (integer?): column offset from the anchor corner.
///   - anchor (string): corner the (row, col) offset is relative to. One of "NW" (default), "NE", "SW", "SE". Or "input_caret", which sits the window beside the chat input caret: the host takes the roomier side of the caret, trims the height to what fits there, keeps the whole width on screen, and re-places it every frame, so it follows wraps, resizes and any modal taking focus. `row` and `col` shift the window off that spot, and `stack` grows the next one away from the caret. With no caret on screen, because a form, a permission prompt or a `below` split has taken the input box, it falls back to the centred default, `row` and `col` still applying.
///   - border (string): border style. One of "rounded" (default), "single", "double", "none".
///   - title (string): text shown in the top border. Default "".
///   - title_pos (string): title alignment. One of "left" (default), "center", "right".
///   - footer (table): key-hint pairs shown in the bottom border. Each entry is {key, label}. A bordered float is widened to fit its title and footer, up to the screen width.
///   - zindex (integer): stacking order. Default 50.
///   - cursor_line (boolean): highlight the focused row. Default false.
///   - reserved_top (integer): rows reserved at the top of the content area. Default 0.
///   - reserved_bottom (integer): rows reserved at the bottom of the content area. Default 0.
///   - split (string): dock the window to an edge instead of floating. One of "above", "below", "left", "right", "panel", or "" (floating, default).
///   - order (integer): paint order among split windows at the same edge. Default 50.
///   - focus (boolean): whether the window takes keyboard focus on open. Default true.
///   - keys (table): keys this window takes while it is on screen, in `maki.keymap` notation, e.g. `{ "<Tab>", "<CR>" }`. `maki.keymap.set` with `{ buffer = buf }` is the newer way to give a popup keys, with a handler per key, a mode, and `ev.fallback()`. Requires `focus = false`, since a focused window already gets every key. A claimed key goes to this window's `recv` and never reaches the chat input or global `maki.keymap.set` bindings. The window's buffer bindings are asked first. Claims are released automatically when the window closes, and a hidden or zero-size window claims nothing. Host pickers and the slash command palette take keys first while open over the window. `<C-c>` and `<C-z>` are refused.
///   - visible (boolean): whether the window is initially visible. Default true. See `win:hide()` for what hiding does.
///   - needs_input (boolean): whether the window means the session needs user input. Default false.
///   - stack (boolean): offset the window past the other stacked windows sharing its anchor, in open order, with a one row gap. Closing one moves the rest up. Floating windows only. Default false.
/// @return (Win) Window handle.
/// @example
/// local buf = maki.ui.buf()
/// buf:line("Pick an option:")
/// local win = maki.ui.open_win(buf, {
///   title = "Menu",
///   width = "50%",
///   height = 10,
///   cursor_line = true,
///   footer = { { "q", "quit" }, { "Enter", "select" } },
/// })
#[lua_fn]
fn open_win(
    lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    buf: mlua::AnyUserData,
    opts: Table,
) -> LuaResult<WinHandle> {
    let buf_handle = buf.borrow::<buf::BufHandle>()?;
    let title: String = opts.get("title").unwrap_or_default();
    let cursor_line = opts.opt_bool("cursor_line").unwrap_or(false);
    let footer = parse_footer(&opts)?;
    let reserved_bottom: usize = opts.get("reserved_bottom").unwrap_or(0);
    let reserved_top: usize = opts.get("reserved_top").unwrap_or(0);
    let focus = opts.opt_bool("focus").unwrap_or(true);
    let keys = parse_claimed_keys(&opts, focus)?;
    let zindex: u16 = opts.get("zindex").unwrap_or(50);

    let width = parse_dimension(&opts, "width", Dimension::Percent(60));
    let height = parse_dimension(&opts, "height", Dimension::Percent(70));
    let row: Option<i16> = opts.opt("row").ok().flatten();
    let col: Option<i16> = opts.opt("col").ok().flatten();
    let anchor = parse_anchor(&opts);
    let border = parse_border(&opts);
    let title_pos = parse_title_pos(&opts);
    let split = parse_split(&opts);
    let order: u16 = opts.get("order").unwrap_or(50);
    let visible = opts.opt_bool("visible").unwrap_or(true);
    let needs_input = opts.opt_bool("needs_input").unwrap_or(false);
    let stack = opts.opt_bool("stack").unwrap_or(false);

    let config = FloatConfig {
        width,
        height,
        row,
        col,
        anchor,
        border,
        title,
        title_pos,
        footer,
        zindex,
        cursor_line,
        reserved_bottom,
        reserved_top,
        split,
        order,
        visible,
        needs_input,
        stack,
        keys,
    };

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let border_chrome = match config.border {
        Border::None => 0,
        _ => 2,
    };
    let est_w = config
        .width
        .resolve(term_cols)
        .saturating_sub(border_chrome);
    let est_h = config
        .height
        .resolve(term_rows)
        .saturating_sub(border_chrome);

    // Unbounded on both sides: a full channel would silently drop keys or,
    // worse, a Close command, leaving a zombie modal. Producers are human- or
    // plugin-rate and both ends are drained every tick, so growth is bounded
    // in practice.
    let (event_tx, event_rx) = flume::unbounded::<WinEvent>();
    let (cmd_tx, cmd_rx) = flume::unbounded::<WinCommand>();
    let waker = lua.app_data_ref::<UiWaker>().map(|waker| waker.clone());
    if let Some(waker) = &waker {
        buf_handle.buf.wake_on_change(waker);
    }
    let cmd_tx = WinSender::new(cmd_tx, waker);

    let _ = ui_send(
        tx.as_ref(),
        UiAction::OpenWin {
            buf: buf_handle.buf.clone(),
            config,
            focus,
            event_tx,
            cmd_rx,
        },
    );

    // Stamped with the plugin that opened it so unloading that plugin closes
    // it, the way its keymaps and hints are cleared. Without the stamp the
    // host has no name on the window and nothing to revoke.
    if let Some(mut store) = lua.app_data_mut::<WinStore>() {
        store.track(plugin, cmd_tx.clone());
    }

    Ok(WinHandle::new(event_rx, cmd_tx, est_w, est_h, visible))
}

#[allow(non_upper_case_globals)]
pub(crate) const set_status_hint__doc: FnDoc = FnDoc {
    name: "set_status_hint",
    args: "{spans}",
    desc: "Shows key hints in the status bar for your plugin. Each hint is a {key, label} pair. Pass nil to clear your plugin's hints. Only your own hints are affected, other plugins keep theirs.",
    params: &[ParamDoc {
        name: "{spans}",
        ty: "table|nil",
        desc: "Sequence of {key, label} pairs, e.g. `{{\"q\", \"quit\"}, {\"j\", \"down\"}}`. Pass nil to remove the plugin's hints.",
    }],
    returns: "",
    guard: None,
    value: false,
    example: "maki.ui.set_status_hint({ {\"q\", \"quit\"}, {\"j\", \"down\"} })\n-- later, clear them:\nmaki.ui.set_status_hint(nil)",
};

lua_table! {
    /// Functions for building interactive UI. Create buffers to hold
    /// content, open floating or split windows to display them, highlight
    /// code, render markdown, and show status hints.
    ///
    /// ```lua
    /// local buf = maki.ui.buf()
    /// buf:line("hello from my plugin!")
    /// local win = maki.ui.open_win(buf, { title = "Greeting", width = "50%", height = 5 })
    /// ```
    ///
    /// Without a UI (`maki -p`, the sdk, ACP), buffers and the text helpers
    /// still work. The calls that need a screen behave like this:
    ///
    /// - `input`, `input_edit`, and `action` on a built-in name return
    ///   `nil, "no interactive UI attached"`. Plugin actions still run.
    /// - `open_editor` returns -1.
    /// - `flash` writes to the log.
    /// - `open_win`, `set_status_hint`, `set_window_title`, `set_mode` and `input_rules` have no effect.
    ///   `input_rules` still throws on a bad rule, so a plugin's tests catch it.
    ///
    /// ### Chat input
    ///
    /// Plugins can read, change and style what the user types in the chat
    /// input. Pick the call by the job:
    ///
    /// | Job | Call |
    /// | --- | --- |
    /// | Read the text and the cursor | [`input`](#maki-ui-input) |
    /// | Style text by what it says, like a `$skill:go` chip | [`input_rules`](#maki-ui-input_rules) |
    /// | Change the text or the cursor | [`input_edit`](#maki-ui-input_edit) |
    /// | React when the input changes | the `InputChanged` [event](#maki-api-create_autocmd) |
    ///
    /// `input` and `input_edit` wait for the UI to answer. They work from any
    /// callback, like a key handler, an autocmd or `maki.defer_fn`, but not
    /// while your plugin loads.
    ///
    /// Changing the chat input needs the `input` permission. That covers its
    /// text and its cursor, since your next Enter sends the text and your
    /// next key lands at the cursor. It covers `input_edit` and the
    /// built-in [actions](#maki-ui-action) that change the input, like
    /// `undo` or a history recall. Without `input` they raise
    /// `permission denied`. A plugin without it can still read the input and
    /// style it with `input_rules`. Your own `init.lua` needs a `plugin.toml`
    /// next to it for this, like any plugin, see
    /// [plugin permissions](#plugin-permissions).
    ///
    /// With `input` you may write any text. Enter then checks every plugin
    /// that changed it:
    ///
    /// | Text starts with | Each plugin that wrote it needs |
    /// | --- | --- |
    /// | `!` | `run` |
    /// | a `/` command | what that command needs from a plugin, see [`maki.api.run_command`](#maki-api-run_command). Only the user can run some commands, like `/yolo` |
    /// | anything else | nothing beyond `input` |
    ///
    /// If one of them falls short, Enter leaves the text where it is and the
    /// status bar names it. The text becomes the user's again only
    /// once the input is sent, cleared or replaced.
    ///
    /// A draft restored after a restart and a rewound or forked turn that
    /// starts with `/` or `!` count as written by `a plugin` that holds
    /// `agent` and `session_write` at most.
    ///
    /// Upgrading: `input_edit` needed no permission in earlier releases.
    /// Plugins and `init.lua` files that call it or an editing built-in
    /// action need `input` now. A `plugin.toml` you wrote yourself grants it
    /// unless it sets `input = false`. A package needs its author to add
    /// `input`, and you to approve the package again.
    extend "maki.ui" => pub(crate) fn add_ui_fns(), DOCS [
        buf, theme_color, theme_style, highlight, markdown, humantime, terminal_size,
        display_width, truncate_text,
        manual set_mode, manual flash, manual action, manual open_editor, manual open_win, manual set_status_hint,
        manual set_window_title, manual input, manual input_edit, manual input_rules,
    ]
}

pub(crate) fn create_ui_table(
    lua: &Lua,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    plugin: Arc<str>,
    reach: PluginReach,
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    add_ui_fns(&t, lua)?;

    flash__register(&t, lua, ui_action_tx.clone(), Arc::clone(&plugin))?;
    set_window_title__register(&t, lua, ui_action_tx.clone())?;
    set_mode__register(&t, lua, Arc::clone(&plugin))?;
    action__register(
        &t,
        lua,
        ui_action_tx.clone(),
        Arc::clone(&plugin),
        reach.clone(),
    )?;
    open_editor__register(&t, lua, ui_action_tx.clone())?;
    input__register(&t, lua, reach.permissions(), ui_action_tx.clone())?;
    input_edit__register(
        &t,
        lua,
        reach.permissions(),
        ui_action_tx.clone(),
        reach.clone(),
    )?;
    input_rules__register(&t, lua, Arc::clone(&plugin))?;
    open_win__register(&t, lua, ui_action_tx, Arc::clone(&plugin))?;

    let p = Arc::clone(&plugin);
    t.set(
        "set_status_hint",
        lua.create_function(move |lua, value: mlua::Value| {
            match value {
                mlua::Value::Nil => {
                    if let Some(mut store) = lua.app_data_mut::<HintStore>() {
                        store.clear_plugin(&p);
                    }
                }
                mlua::Value::Table(tbl) => {
                    let spans: Vec<(String, String)> = tbl
                        .sequence_values::<Table>()
                        .map(|entry| {
                            let entry = entry?;
                            Ok((
                                entry.get::<String>(1)?,
                                entry.get::<String>(2).unwrap_or_default(),
                            ))
                        })
                        .collect::<LuaResult<_>>()?;
                    if let Some(mut store) = lua.app_data_mut::<HintStore>() {
                        store.set(Arc::clone(&p), spans);
                    }
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "set_status_hint expects a table or nil",
                    ));
                }
            }
            publish_hint_snapshot(lua);
            Ok(())
        })?,
    )?;

    Ok(t)
}

pub(crate) fn try_parse_dimension(tbl: &Table, key: &str) -> Option<Dimension> {
    if let Ok(s) = tbl.get::<String>(key)
        && let Some(pct) = s.strip_suffix('%')
        && let Ok(v) = pct.parse::<u16>()
    {
        return Some(Dimension::Percent(v));
    }
    if let Ok(v) = tbl.get::<u16>(key) {
        return Some(Dimension::Abs(v));
    }
    None
}

pub(crate) fn parse_dimension(tbl: &Table, key: &str, default: Dimension) -> Dimension {
    try_parse_dimension(tbl, key).unwrap_or(default)
}

fn parse_anchor(tbl: &Table) -> Anchor {
    tbl.get::<String>("anchor")
        .map(|s| Anchor::parse(&s))
        .unwrap_or_default()
}

fn parse_split(tbl: &Table) -> Split {
    tbl.get::<String>("split")
        .map(|s| Split::parse(&s))
        .unwrap_or_default()
}

fn parse_border(tbl: &Table) -> Border {
    tbl.get::<String>("border")
        .map(|s| Border::parse(&s))
        .unwrap_or_default()
}

fn parse_title_pos(tbl: &Table) -> TitlePos {
    tbl.get::<String>("title_pos")
        .map(|s| TitlePos::parse(&s))
        .unwrap_or_default()
}

/// Palette colors stay symbolic (`"4"`, `"12"`, `"default"`) so the terminal
/// picks the actual shade; only true RGB is written as hex. Every spelling
/// here round-trips back through a span's `fg`/`bg`.
fn segment_color_to_lua(c: SegmentColor) -> String {
    match c {
        SegmentColor::Rgb((r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        SegmentColor::Ansi(i) => i.to_string(),
        SegmentColor::Default => DEFAULT_COLOR_NAME.to_owned(),
    }
}

fn segments_to_lua_lines(
    lua: &Lua,
    lines: &[Vec<maki_highlight::StyledSegment>],
) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, segs) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(segs.len(), 0)?;
        for (j, seg) in segs.iter().enumerate() {
            let span = lua.create_table_with_capacity(2, 0)?;
            span.raw_set(1, seg.text.as_str())?;
            let style = lua.create_table_with_capacity(0, 4)?;
            style.raw_set("fg", segment_color_to_lua(seg.fg))?;
            if seg.bold {
                style.raw_set("bold", true)?;
            }
            if seg.italic {
                style.raw_set("italic", true)?;
            }
            if seg.underline {
                style.raw_set("underline", true)?;
            }
            span.raw_set(2, style)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

/// Most spans become a named style string. `Highlight` tokens carry their
/// own rgb, so they become an inline `{fg, bold, italic, underline}` table.
///
/// No wildcard arm on `StyleToken`: adding a variant is a compile error
/// here, so we can't forget to map it.
fn span_style_to_lua(lua: &Lua, span: &maki_markdown::render::Span) -> LuaResult<mlua::Value> {
    use maki_markdown::render::StyleToken;

    let v = match &span.style {
        StyleToken::Text => {
            let name = emphasis_style_name(span.emphasis);
            mlua::Value::String(lua.create_string(name)?)
        }
        StyleToken::InlineCode => mlua::Value::String(lua.create_string("inline_code")?),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let tbl = lua.create_table()?;
            tbl.set("fg", segment_color_to_lua(*fg))?;
            if *bold {
                tbl.set("bold", true)?;
            }
            if *italic {
                tbl.set("italic", true)?;
            }
            if *underline {
                tbl.set("underline", true)?;
            }
            mlua::Value::Table(tbl)
        }
        StyleToken::CodeBar => mlua::Value::String(lua.create_string("code_gutter")?),
        StyleToken::Heading => mlua::Value::String(lua.create_string("heading")?),
        StyleToken::ListMarker => mlua::Value::String(lua.create_string("list_marker")?),
        StyleToken::TableBorder => mlua::Value::String(lua.create_string("table_border")?),
        StyleToken::HorizontalRule => mlua::Value::String(lua.create_string("horizontal_rule")?),
    };
    Ok(v)
}

/// Flatten `Emphasis` to a single named style. Strike wins over bold/italic
/// (the Lua theme has no combined slot). Underline only appears in
/// `Highlight` tokens, not here.
fn emphasis_style_name(e: maki_markdown::Emphasis) -> &'static str {
    if e.strike {
        "strikethrough"
    } else if e.bold && e.italic {
        "bold_italic"
    } else if e.bold {
        "bold"
    } else if e.italic {
        "italic"
    } else {
        ""
    }
}

fn markdown_lines_to_lua(lua: &Lua, lines: &[maki_markdown::render::Line]) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, rendered) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(rendered.spans.len(), 0)?;
        for (j, sp) in rendered.spans.iter().enumerate() {
            let span_tbl = lua.create_table_with_capacity(2, 0)?;
            span_tbl.raw_set(1, sp.text.as_str())?;
            span_tbl.raw_set(2, span_style_to_lua(lua, sp)?)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span_tbl)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::UiReply;
    use crate::api::util::convert::{UNKNOWN_FIELD_ERR, lua_to_json};
    use crate::plugin_permissions::{Permission, PluginPermissions, denied_error};
    use maki_highlight::StyledSegment;
    use mlua::Lua;
    use test_case::test_case;

    const MISSING_KEY: &str = "missing";
    const ORANGE_HEX: &str = "#ff8000";

    fn footer_entry(lua: &Lua, key: &str, label: &str) -> Table {
        let t = lua.create_table().unwrap();
        t.raw_set(1, key).unwrap();
        t.raw_set(2, label).unwrap();
        t
    }

    #[test]
    fn parse_footer_missing_returns_empty() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert!(parse_footer(&tbl).unwrap().is_empty());
    }

    #[test]
    fn parse_footer_non_table_value_returns_empty() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("footer", "not a table").unwrap();
        assert!(parse_footer(&tbl).unwrap().is_empty());
    }

    #[test]
    fn parse_footer_preserves_entry_order() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        entries.raw_set(1, footer_entry(&lua, "q", "quit")).unwrap();
        entries.raw_set(2, footer_entry(&lua, "j", "down")).unwrap();
        entries.raw_set(3, footer_entry(&lua, "k", "up")).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        let parsed = parse_footer(&tbl).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("q".into(), "quit".into()),
                ("j".into(), "down".into()),
                ("k".into(), "up".into()),
            ]
        );
    }

    #[test]
    fn parse_footer_missing_label_errors() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        let one_elem = lua.create_table().unwrap();
        one_elem.raw_set(1, "q").unwrap();
        entries.raw_set(1, one_elem).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        assert!(parse_footer(&tbl).is_err());
    }

    #[test]
    fn parse_footer_non_string_element_errors() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        let bad = lua.create_table().unwrap();
        bad.raw_set(1, "q").unwrap();
        bad.raw_set(2, lua.create_table().unwrap()).unwrap();
        entries.raw_set(1, bad).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        assert!(parse_footer(&tbl).is_err());
    }

    /// A window with {claims} in its `keys`, as `open_win` reads it.
    fn claim_opts(lua: &Lua, claims: &[&str]) -> Table {
        let opts = lua.create_table().unwrap();
        let keys = lua.create_table().unwrap();
        for (i, key) in claims.iter().enumerate() {
            keys.raw_set(i + 1, *key).unwrap();
        }
        opts.raw_set("keys", keys).unwrap();
        opts
    }

    #[test]
    fn claimed_keys_are_parsed_in_the_notation_keymaps_use() {
        let lua = Lua::new();
        let opts = claim_opts(&lua, &["<Tab>", "<C-n>"]);

        assert_eq!(
            parse_claimed_keys(&opts, false).unwrap(),
            vec![Key::parse("<Tab>").unwrap(), Key::parse("<C-n>").unwrap()]
        );
    }

    #[test]
    fn a_window_with_no_keys_claims_none() {
        let lua = Lua::new();
        let opts = lua.create_table().unwrap();

        assert!(parse_claimed_keys(&opts, true).unwrap().is_empty());
    }

    /// The same two keys `maki.keymap.set` refuses, from the same list. A
    /// window claiming one would publish a key the host answers first, which
    /// is a popup that never sees it and a user who cannot quit.
    #[test_case("<C-c>" ; "quit")]
    #[test_case("<C-z>" ; "suspend")]
    fn a_window_cannot_claim_a_key_the_host_reserves(lhs: &str) {
        let lua = Lua::new();
        let opts = claim_opts(&lua, &[lhs]);

        let err = parse_claimed_keys(&opts, false).unwrap_err().to_string();
        assert!(
            err.contains(lhs),
            "the error has to name the key, got: {err}"
        );
    }

    /// A focused window is handed every key already, so a list of keys to take
    /// on top of that is an author with the wrong model of who is reading the
    /// keyboard, not a redundant option to quietly drop.
    #[test]
    fn claiming_keys_on_a_focused_window_is_an_error() {
        let lua = Lua::new();
        let opts = claim_opts(&lua, &["<Tab>"]);

        let err = parse_claimed_keys(&opts, true).unwrap_err().to_string();
        assert!(err.contains(FOCUSED_CLAIM_ERR), "got: {err}");
    }

    #[test]
    fn try_parse_dimension_numeric_is_abs() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", 42u16).unwrap();
        assert_eq!(try_parse_dimension(&tbl, "width"), Some(Dimension::Abs(42)));
    }

    #[test_case("0%", Dimension::Percent(0) ; "zero_percent")]
    #[test_case("50%", Dimension::Percent(50) ; "half_percent")]
    #[test_case("100%", Dimension::Percent(100) ; "full_percent")]
    #[test_case("200%", Dimension::Percent(200) ; "over_hundred_accepted")]
    fn try_parse_dimension_percent_strings(input: &str, expected: Dimension) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", input).unwrap();
        assert_eq!(try_parse_dimension(&tbl, "width"), Some(expected));
    }

    #[test]
    fn try_parse_dimension_missing_key_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert!(try_parse_dimension(&tbl, MISSING_KEY).is_none());
    }

    #[test]
    fn try_parse_dimension_non_numeric_string_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "abc").unwrap();
        assert!(try_parse_dimension(&tbl, "width").is_none());
    }

    #[test]
    fn try_parse_dimension_malformed_percent_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "xx%").unwrap();
        assert!(try_parse_dimension(&tbl, "width").is_none());
    }

    #[test]
    fn parse_dimension_missing_key_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let default = Dimension::Percent(60);
        assert_eq!(parse_dimension(&tbl, MISSING_KEY, default), default);
    }

    #[test]
    fn parse_dimension_invalid_value_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "garbage").unwrap();
        let default = Dimension::Abs(80);
        assert_eq!(parse_dimension(&tbl, "width", default), default);
    }

    #[test_case("NW", Anchor::NW ; "nw")]
    #[test_case("NE", Anchor::NE ; "ne")]
    #[test_case("SW", Anchor::SW ; "sw")]
    #[test_case("SE", Anchor::SE ; "se")]
    #[test_case("input_caret", Anchor::InputCaret ; "input_caret")]
    #[test_case("garbage", Anchor::NW ; "invalid_falls_back_to_default")]
    fn parse_anchor_cases(input: &str, expected: Anchor) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("anchor", input).unwrap();
        assert_eq!(parse_anchor(&tbl), expected);
    }

    #[test]
    fn parse_anchor_missing_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert_eq!(parse_anchor(&tbl), Anchor::default());
    }

    #[test_case("none", Border::None ; "none")]
    #[test_case("single", Border::Single ; "single")]
    #[test_case("double", Border::Double ; "double")]
    #[test_case("rounded", Border::Rounded ; "rounded")]
    #[test_case("garbage", Border::Rounded ; "invalid_falls_back_to_default")]
    fn parse_border_cases(input: &str, expected: Border) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("border", input).unwrap();
        assert_eq!(parse_border(&tbl), expected);
    }

    #[test]
    fn parse_border_missing_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert_eq!(parse_border(&tbl), Border::default());
    }

    #[test_case("left", TitlePos::Left ; "left")]
    #[test_case("center", TitlePos::Center ; "center")]
    #[test_case("right", TitlePos::Right ; "right")]
    #[test_case("garbage", TitlePos::Left ; "invalid_falls_back_to_default")]
    fn parse_title_pos_cases(input: &str, expected: TitlePos) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("title_pos", input).unwrap();
        assert_eq!(parse_title_pos(&tbl), expected);
    }

    fn seg(text: &str, bold: bool) -> StyledSegment {
        StyledSegment {
            text: text.into(),
            fg: SegmentColor::Rgb((255, 128, 0)),
            bold,
            italic: false,
            underline: false,
        }
    }

    #[test]
    fn segments_to_lua_lines_empty_input() {
        let lua = Lua::new();
        let result = segments_to_lua_lines(&lua, &[]).unwrap();
        assert_eq!(result.len().unwrap(), 0);
    }

    #[test]
    fn segments_to_lua_lines_shape_and_fg_hex() {
        let lua = Lua::new();
        let lines = vec![vec![seg("fn ", true), seg("main", false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();

        assert_eq!(result.len().unwrap(), 1);
        let line: Table = result.get(1).unwrap();
        assert_eq!(line.len().unwrap(), 2);

        let span: Table = line.get(1).unwrap();
        let text: String = span.get(1).unwrap();
        assert_eq!(text, "fn ");
        let style: Table = span.get(2).unwrap();
        let fg: String = style.get("fg").unwrap();
        assert_eq!(fg, ORANGE_HEX);
        let bold: bool = style.get("bold").unwrap();
        assert!(bold);
        assert!(style.get::<Option<bool>>("italic").unwrap().is_none());

        let span2: Table = line.get(2).unwrap();
        let text2: String = span2.get(1).unwrap();
        assert_eq!(text2, "main");
        let style2: Table = span2.get(2).unwrap();
        assert!(style2.get::<Option<bool>>("bold").unwrap().is_none());
    }

    #[test]
    fn segments_to_lua_lines_preserves_utf8() {
        let lua = Lua::new();
        let utf8 = "héllo 🦀 ✨";
        let lines = vec![vec![seg(utf8, false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        let line: Table = result.get(1).unwrap();
        let span: Table = line.get(1).unwrap();
        let text: String = span.get(1).unwrap();
        assert_eq!(text, utf8);
    }

    const STYLE_BOLD: &str = "bold";
    const STYLE_BOLD_ITALIC: &str = "bold_italic";
    const STYLE_HEADING: &str = "heading";
    const STYLE_LIST_MARKER: &str = "list_marker";
    const STYLE_HR: &str = "horizontal_rule";
    const STYLE_PLAIN: &str = "";
    const STYLE_CODE: &str = "inline_code";
    const STYLE_CODE_BAR: &str = "code_gutter";
    const STYLE_ITALIC: &str = "italic";
    const STYLE_STRIKE: &str = "strikethrough";
    const STYLE_TABLE_BORDER: &str = "table_border";
    const MD_WIDTH: u16 = 80;

    fn render_markdown(lua: &Lua, input: &str) -> Table {
        let lines = maki_markdown::render::render(input, MD_WIDTH);
        markdown_lines_to_lua(lua, &lines).unwrap()
    }

    fn span_style(line: &Table, idx: usize) -> String {
        let span: Table = line.get(idx).unwrap();
        match span.get::<mlua::Value>(2).unwrap() {
            mlua::Value::String(s) => s.to_str().unwrap().to_string(),
            other => panic!("expected string style, got {other:?}"),
        }
    }

    fn span_text(line: &Table, idx: usize) -> String {
        let span: Table = line.get(idx).unwrap();
        span.get::<String>(1).unwrap()
    }

    #[test]
    fn markdown_returns_named_styles() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "hello **world**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "hello ");
        assert_eq!(span_style(&line, 1), STYLE_PLAIN);
        assert_eq!(span_text(&line, 2), "world");
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
    }

    #[test]
    fn markdown_bold_italic_emits_bold_italic_not_collapsed_to_bold() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "***x***");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_BOLD_ITALIC);
    }

    #[test]
    fn markdown_unknown_constructs_fall_through_as_plain() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "a*b");
        let line: Table = result.get(1).unwrap();
        for i in 1..=line.len().unwrap() {
            assert_eq!(span_style(&line, i as usize), STYLE_PLAIN);
        }
    }

    #[test]
    fn markdown_code_span_uses_inline_code_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "x `y` z");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 2), STYLE_CODE);
    }

    #[test]
    fn markdown_heading_overrides_inline_emphasis_with_heading_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# hello **world**");
        let line: Table = result.get(1).unwrap();
        for i in 1..=line.len().unwrap() {
            assert_eq!(
                span_style(&line, i as usize),
                STYLE_HEADING,
                "span {i} should be heading-styled"
            );
        }
    }

    #[test]
    fn markdown_list_marker_styled_separately_from_item_content() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "- **item**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_text(&line, 1), "• ");
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
        assert_eq!(span_text(&line, 2), "item");
    }

    #[test]
    fn markdown_horizontal_rule_emits_hr_span_filled_to_width() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "---");
        let line: Table = result.get(1).unwrap();
        assert_eq!(line.len().unwrap(), 1);
        assert_eq!(span_style(&line, 1), STYLE_HR);
        let text = span_text(&line, 1);
        assert_eq!(text.chars().count(), MD_WIDTH as usize);
        assert!(text.chars().all(|c| c == '─'));
    }

    #[test]
    fn markdown_code_inside_bold_collapses_to_inline_code_at_lua_boundary() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "**`code`**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_CODE);
    }

    #[test]
    fn markdown_multiline_emits_one_lua_line_per_logical_line() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "line one\nline two\nline three");
        assert_eq!(result.len().unwrap(), 3);
        let l1: Table = result.get(1).unwrap();
        let l2: Table = result.get(2).unwrap();
        let l3: Table = result.get(3).unwrap();
        assert_eq!(span_text(&l1, 1), "line one");
        assert_eq!(span_text(&l2, 1), "line two");
        assert_eq!(span_text(&l3, 1), "line three");
    }

    #[test]
    fn markdown_italic_alone_surfaces_as_italic_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "*italic*");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "italic");
        assert_eq!(span_style(&line, 1), STYLE_ITALIC);
    }

    #[test]
    fn markdown_strikethrough_surfaces_as_strikethrough_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "~~gone~~");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "gone");
        assert_eq!(span_style(&line, 1), STYLE_STRIKE);
    }

    #[test]
    fn markdown_ordered_list_marker_text_and_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "1. foo");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "1. ");
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_text(&line, 2), "foo");
        assert_eq!(span_style(&line, 2), STYLE_PLAIN);
    }

    #[test]
    fn markdown_ordered_list_marker_keeps_list_marker_style_with_bold_content() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "1. **item**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
        assert_eq!(span_text(&line, 2), "item");
    }

    #[test]
    fn markdown_code_fence_emits_code_bar_prefix_with_highlight_span_tables() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "```rust\nfn x() {}\n```");
        let lines = result.len().unwrap();
        let code_line: Table = (1..=lines)
            .find_map(|i| {
                let line: Table = result.get(i).ok()?;
                (line.len().ok()? > 0
                    && line
                        .get::<Table>(1)
                        .and_then(|s| s.get::<String>(2))
                        .ok()
                        .is_some_and(|s| s == STYLE_CODE_BAR))
                .then_some(line)
            })
            .expect("code bar line");
        assert_eq!(span_style(&code_line, 1), STYLE_CODE_BAR);
        let content_span: Table = code_line.get(2).unwrap();
        let style = content_span.get::<mlua::Value>(2).unwrap();
        assert!(
            matches!(style, mlua::Value::Table(_)),
            "highlight span style must be an inline table"
        );
    }

    #[test_case("# a" ; "h1")]
    #[test_case("## a" ; "h2")]
    #[test_case("### a" ; "h3")]
    #[test_case("###### a" ; "h6")]
    fn markdown_heading_levels_all_surface_as_heading_style(input: &str) {
        let lua = Lua::new();
        let result = render_markdown(&lua, input);
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_HEADING);
    }

    fn seg_full(text: &str, bold: bool, italic: bool, underline: bool) -> StyledSegment {
        StyledSegment {
            text: text.into(),
            fg: SegmentColor::Rgb((255, 128, 0)),
            bold,
            italic,
            underline,
        }
    }

    #[test]
    fn segments_to_lua_lines_modifier_flags_only_present_when_true() {
        let lua = Lua::new();
        let lines = vec![vec![
            seg_full("a", false, true, true),
            seg_full("b", false, false, false),
        ]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        let line: Table = result.get(1).unwrap();
        let s1: Table = line.get(1).unwrap();
        let st1: Table = s1.get(2).unwrap();
        assert!(st1.get::<bool>("italic").unwrap());
        assert!(st1.get::<bool>("underline").unwrap());
        let s2: Table = line.get(2).unwrap();
        let st2: Table = s2.get(2).unwrap();
        assert!(st2.get::<Option<bool>>("italic").unwrap().is_none());
        assert!(st2.get::<Option<bool>>("underline").unwrap().is_none());
    }

    #[test]
    fn segments_to_lua_lines_preserves_line_order() {
        let lua = Lua::new();
        let lines = vec![vec![seg("a", false)], vec![seg("b", false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        assert_eq!(result.len().unwrap(), 2);
        let l1: Table = result.get(1).unwrap();
        let l2: Table = result.get(2).unwrap();
        let s1: Table = l1.get(1).unwrap();
        let s2: Table = l2.get(1).unwrap();
        assert_eq!(s1.get::<String>(1).unwrap(), "a");
        assert_eq!(s2.get::<String>(1).unwrap(), "b");
    }

    #[test]
    fn markdown_table_has_border_and_data_spans() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "| col1 | col2 |\n|------|------|\n| a    | b    |");
        let mut saw_border = false;
        let mut saw_plain = false;
        for i in 1..=result.len().unwrap() {
            let line: Table = result.get(i).unwrap();
            for j in 1..=line.len().unwrap() {
                let span: Table = line.get(j).unwrap();
                if let mlua::Value::String(s) = span.get::<mlua::Value>(2).unwrap() {
                    let s = s.to_str().unwrap();
                    if s == STYLE_TABLE_BORDER {
                        saw_border = true;
                    } else if s == STYLE_PLAIN {
                        saw_plain = true;
                    }
                }
            }
        }
        assert!(saw_border, "table must have border spans");
        assert!(saw_plain, "table must have data/content spans");
    }

    #[test]
    fn markdown_large_input_does_not_panic() {
        let lua = Lua::new();
        let mut input = String::with_capacity(2048);
        for i in 0..200 {
            input.push_str(&format!(
                "# h{i}\n\npara **b{i}** *i{i}* `c{i}` ~~s{i}~~\n\n- item {i}\n\n"
            ));
        }
        assert!(input.len() >= 2000);
        let result = render_markdown(&lua, &input);
        assert!(result.len().unwrap() > 0);
    }

    #[test]
    fn markdown_code_inside_heading_keeps_inline_code_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# foo `bar`");
        let line: Table = result.get(1).unwrap();
        let bar_idx = (1..=line.len().unwrap())
            .find(|&i| span_text(&line, i as usize) == "bar")
            .expect("bar span");
        assert_eq!(span_style(&line, bar_idx as usize), STYLE_CODE);
        let foo_idx = (1..=line.len().unwrap())
            .find(|&i| span_text(&line, i as usize).contains("foo"))
            .expect("foo span");
        assert_eq!(span_style(&line, foo_idx as usize), STYLE_HEADING);
    }

    #[test_case(false, false, false, false, "" ; "default_emphasis_is_empty")]
    #[test_case(true, false, false, false, "bold" ; "bold_only")]
    #[test_case(false, true, false, false, "italic" ; "italic_only")]
    #[test_case(true, true, false, false, "bold_italic" ; "bold_and_italic")]
    #[test_case(false, false, true, false, "strikethrough" ; "strike_only")]
    #[test_case(true, false, true, false, "strikethrough" ; "strike_wins_over_bold")]
    #[test_case(false, true, true, false, "strikethrough" ; "strike_wins_over_italic")]
    #[test_case(false, false, false, true, "" ; "underline_alone_not_surfaced")]
    #[test_case(true, false, false, true, "bold" ; "underline_ignored_with_bold")]
    fn emphasis_style_name_combos(
        bold: bool,
        italic: bool,
        strike: bool,
        underline: bool,
        expected: &str,
    ) {
        let e = maki_markdown::Emphasis {
            bold,
            italic,
            strike,
            underline,
        };
        assert_eq!(emphasis_style_name(e), expected);
    }

    #[test]
    fn span_style_to_lua_highlight() {
        let lua = Lua::new();
        for (bold, italic, underline) in [(true, false, false), (true, true, true)] {
            let span = maki_markdown::render::Span {
                text: "tok".into(),
                style: maki_markdown::render::StyleToken::Highlight {
                    fg: SegmentColor::Rgb((255, 128, 0)),
                    bold,
                    italic,
                    underline,
                },
                emphasis: maki_markdown::Emphasis::default(),
            };
            let val = span_style_to_lua(&lua, &span).unwrap();
            let tbl = match val {
                mlua::Value::Table(t) => t,
                other => panic!("expected table, got {other:?}"),
            };
            assert_eq!(tbl.get::<String>("fg").unwrap(), ORANGE_HEX);
            assert_eq!(tbl.get::<bool>("bold").unwrap(), bold);
            assert_eq!(
                tbl.get::<Option<bool>>("italic").unwrap().unwrap_or(false),
                italic
            );
            assert_eq!(
                tbl.get::<Option<bool>>("underline")
                    .unwrap()
                    .unwrap_or(false),
                underline
            );
        }
    }

    #[test]
    fn markdown_mixed_document_routes_styles_per_block_kind() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# Title\n\nBody **bold** here.\n\n- item");
        assert!(result.len().unwrap() >= 5);

        let heading_line: Table = result.get(1).unwrap();
        for i in 1..=heading_line.len().unwrap() {
            assert_eq!(span_style(&heading_line, i as usize), STYLE_HEADING);
        }

        let body_line: Table = result.get(3).unwrap();
        let mut saw_bold = false;
        for i in 1..=body_line.len().unwrap() {
            if span_style(&body_line, i as usize) == STYLE_BOLD {
                assert_eq!(span_text(&body_line, i as usize), "bold");
                saw_bold = true;
            }
        }
        assert!(saw_bold, "body line should contain a bold span");

        let list_line: Table = result.get(5).unwrap();
        assert_eq!(span_style(&list_line, 1), STYLE_LIST_MARKER);
    }

    #[test]
    fn hint_store_set_and_clear() {
        let mut store = HintStore::new();
        store.set(Arc::from("plug"), vec![("a".into(), "b".into())]);
        assert_eq!(store.snapshot_entries().len(), 1);

        store.clear_plugin("plug");
        assert!(store.snapshot_entries().is_empty());
    }

    #[test]
    fn hint_store_deterministic_order() {
        let mut store = HintStore::new();
        store.set(Arc::from("zzz"), vec![("z".into(), "z".into())]);
        store.set(Arc::from("aaa"), vec![("a".into(), "a".into())]);
        let entries = store.snapshot_entries();
        assert_eq!(entries[0].0.as_ref(), "aaa");
        assert_eq!(entries[1].0.as_ref(), "zzz");
    }

    #[test]
    fn hint_store_empty_clears() {
        let mut store = HintStore::new();
        store.set(Arc::from("plug"), vec![("a".into(), "b".into())]);
        store.set(Arc::from("plug"), vec![]);
        assert!(store.snapshot_entries().is_empty());
    }

    const WIN_PLUGIN: &str = "plug";
    const OTHER_WIN_PLUGIN: &str = "other";
    const WINDOW_LEFT_OPEN: &str =
        "unloading a plugin has to close the window it left on screen holding keys";
    const WINDOW_TAKEN_DOWN: &str = "another plugin's window must survive the unload";

    /// What `/reload` costs without this: the float stays up with its key loop
    /// cancelled, so every key it claimed is taken from the user and dropped
    /// until the Lua collector happens to run.
    #[test]
    fn unloading_a_plugin_closes_the_windows_it_opened() {
        let mut store = WinStore::default();
        let (mine, mine_rx) = flume::unbounded::<WinCommand>();
        let (theirs, theirs_rx) = flume::unbounded::<WinCommand>();
        store.track(Arc::from(WIN_PLUGIN), WinSender::new(mine, None));
        store.track(Arc::from(OTHER_WIN_PLUGIN), WinSender::new(theirs, None));

        store.close_plugin(WIN_PLUGIN);

        assert!(
            matches!(mine_rx.try_recv(), Ok(WinCommand::Close)),
            "{WINDOW_LEFT_OPEN}"
        );
        assert!(theirs_rx.try_recv().is_err(), "{WINDOW_TAKEN_DOWN}");
    }

    /// A popup opened and closed on every keystroke must not grow the list it
    /// is tracked in for the rest of the run.
    #[test]
    fn a_window_already_gone_is_forgotten_on_the_next_open() {
        let mut store = WinStore::default();
        let (gone, gone_rx) = flume::unbounded::<WinCommand>();
        store.track(Arc::from(WIN_PLUGIN), WinSender::new(gone, None));
        drop(gone_rx);

        let (live, _live_rx) = flume::unbounded::<WinCommand>();
        store.track(Arc::from(WIN_PLUGIN), WinSender::new(live, None));

        assert_eq!(store.open[WIN_PLUGIN].len(), 1);
    }

    const STALE_RANGE_ERR: &str = "stop 99 is past the end of the input (5)";
    const READ_SESSION_ID: &str = "11111111-1111-1111-1111-111111111111";
    const INPUT_PLUGIN: &str = "test";

    /// Stands in for the focused session's input box: reads answer with a
    /// snapshot, edits answer with whatever {edit} decides.
    fn ui_with_input(edit: fn(InputEdit) -> UiReply) -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        std::thread::spawn(move || {
            while let Ok(UiAction::Input { req, reply_tx }) = rx.recv() {
                let reply = match req {
                    InputRequest::Read => Ok(serde_json::json!({
                        "session_id": READ_SESSION_ID,
                        "text": "hello",
                        "cursor": 5,
                        "version": 7,
                    })),
                    InputRequest::Edit(edit_req) => edit(edit_req),
                };
                let _ = reply_tx.send(reply);
            }
        });
        lua_with_ui(Some(tx))
    }

    fn lua_with_ui(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_ui_table(
            &lua,
            tx,
            Arc::from(INPUT_PLUGIN),
            PluginReach::new(PluginPermissions::trusted()).owned_by(Arc::from(INPUT_PLUGIN)),
        )
        .unwrap();
        lua.globals().set("ui", t).unwrap();
        lua
    }

    /// Echoes the request back, so a test can assert on what the UI would have
    /// been asked to do.
    fn echo_edit(edit: InputEdit) -> UiReply {
        let edits: Vec<_> = edit
            .edits
            .iter()
            .map(|e| serde_json::json!({ "start": e.start, "stop": e.stop, "text": e.text }))
            .collect();
        Ok(serde_json::json!({
            "edits": edits,
            "cursor": edit.cursor,
            "version": edit.version,
            "strict": edit.strict,
            "session_id": edit.session_id,
            "plugin": edit.caller.owner(),
        }))
    }

    /// Both guards are required, so a test only ever varies the range.
    fn edit_script(range: &str) -> String {
        format!(
            r#"local st = ui.input()
               return ui.input_edit({{
                 {range},
                 version = st.version, session_id = st.session_id,
               }})"#
        )
    }

    fn eval(lua: &Lua, script: &str) -> (serde_json::Value, Option<String>) {
        let (val, err): (mlua::Value, Option<String>) =
            smol::block_on(lua.load(script).eval_async()).unwrap();
        (lua_to_json(lua, &val).unwrap(), err)
    }

    fn eval_err(lua: &Lua, script: &str) -> String {
        smol::block_on(lua.load(script).eval_async::<mlua::Value>())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn input_reports_text_cursor_and_version() {
        let lua = ui_with_input(echo_edit);
        let (val, err) = eval(&lua, "return ui.input()");
        assert_eq!(err, None);
        assert_eq!(val["text"], "hello");
        assert_eq!(val["cursor"], 5);
        assert_eq!(val["version"], 7);
        assert_eq!(val["session_id"], READ_SESSION_ID);
    }

    #[test_case(
        r#"start = 1, stop = 3, text = "xy""#,
        serde_json::json!({ "edits": [{ "start": 1, "stop": 3, "text": "xy" }], "cursor": null, "strict": false })
        ; "single_edit_with_defaults"
    )]
    #[test_case(
        r#"edits = { { start = 0, stop = 0, text = "**" }, { start = 5, stop = 5, text = "**" } }, cursor = 7, strict = true"#,
        serde_json::json!({
            "edits": [{ "start": 0, "stop": 0, "text": "**" }, { "start": 5, "stop": 5, "text": "**" }],
            "cursor": 7,
            "strict": true,
        })
        ; "edits_list_with_options"
    )]
    #[test_case(
        "edits = {}, cursor = 2",
        serde_json::json!({ "edits": [], "cursor": 2, "strict": false })
        ; "no_edits_with_a_cursor"
    )]
    fn input_edit_forwards_the_edits_and_options(fields: &str, expected: serde_json::Value) {
        let lua = ui_with_input(echo_edit);
        let (val, err) = eval(&lua, &edit_script(fields));
        assert_eq!(err, None);
        assert_eq!(
            serde_json::json!({ "edits": val["edits"], "cursor": val["cursor"], "strict": val["strict"] }),
            expected
        );
    }

    /// Only the list part of `edits` is read, so a single edit in its place
    /// would read as no edits and the call would just move the cursor.
    #[test_case(r#"edits = { start = 0, stop = 0, text = "x" }, cursor = 1"# ; "a_single_edit_in_place_of_the_list")]
    #[test_case(r#"edits = { { start = 0, stop = 0, text = "x" }, cursor = 1 }"# ; "an_option_inside_the_list")]
    fn input_edit_refuses_edits_that_are_not_a_list(fields: &str) {
        let lua = ui_with_input(echo_edit);
        let err = eval_err(&lua, &edit_script(fields));
        assert!(err.contains(NOT_A_LIST_ERR), "{err}");
    }

    /// Mixing both shapes would leave one of them silently unapplied.
    #[test]
    fn input_edit_refuses_an_edits_list_next_to_a_single_edit() {
        let lua = ui_with_input(echo_edit);
        let err = eval_err(
            &lua,
            &edit_script(r#"edits = { { start = 0, stop = 0, text = "a" } }, text = "b""#),
        );
        assert!(err.contains(MIXED_EDIT_SHAPES_ERR), "{err}");
    }

    /// A misspelled option would quietly drop what it asks for, like
    /// `strict` keeping a stale edit from landing.
    #[test_case(r#"start = 0, stop = 0, text = "x", strcit = true"# ; "a_misspelled_option")]
    #[test_case(r#"edits = { { start = 0, stop = 0, text = "x", cursor = 1 } }"# ; "an_option_inside_an_edit")]
    fn input_edit_throws_on_an_unknown_field(fields: &str) {
        let lua = ui_with_input(echo_edit);
        let err = eval_err(&lua, &edit_script(fields));
        assert!(err.contains(UNKNOWN_FIELD_ERR), "{err}");
    }

    /// Leaving a required key out has to fail loudly instead of writing
    /// unguarded, and that includes {text}: the input box has no undo, so a
    /// misspelled key defaulting to the empty string deletes the range the
    /// call meant to replace.
    #[test_case("version" ; "version")]
    #[test_case("session_id" ; "session_id")]
    #[test_case("text" ; "text")]
    fn input_edit_refuses_to_write_without_a_required_key(missing: &str) {
        let lua = ui_with_input(echo_edit);
        let kept = [
            "version = st.version",
            "session_id = st.session_id",
            r#"text = "x""#,
        ]
        .iter()
        .filter(|key| !key.starts_with(missing))
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
        let err = eval_err(
            &lua,
            &format!(
                r#"local st = ui.input()
                   return ui.input_edit({{ start = 0, stop = 5, {kept} }})"#
            ),
        );
        assert!(err.contains(missing), "the error has to name it: {err}");
    }

    /// A handler writes after the key that woke it, so a range the user has
    /// typed past comes back as an error instead of landing somewhere else.
    #[test]
    fn input_edit_answers_a_stale_range_in_the_error_slot() {
        let lua = ui_with_input(|_| Err(STALE_RANGE_ERR.into()));
        let (val, err) = eval(&lua, &edit_script("start = 0, stop = 99, text = \"x\""));
        assert_eq!(val, serde_json::Value::Null);
        assert_eq!(err.as_deref(), Some(STALE_RANGE_ERR));
    }

    /// An action is priced like the API doing the same thing, so binding a
    /// key to it cannot reach further than calling that API would.
    #[test_case("plan_toggle", None ; "plan_toggle_is_free")]
    #[test_case("pop_queue", Some(Permission::SessionWrite) ; "pop_queue_needs_session_write")]
    #[test_case("file_picker", None ; "file_picker_is_free")]
    fn action_charges_what_its_api_would(name: &str, needed: Option<Permission>) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        std::thread::spawn(move || {
            while let Ok(action) = rx.recv() {
                if let UiAction::Builtin { reply_tx, .. } = action {
                    let _ = reply_tx.send(true);
                }
            }
        });
        let run = |perms: &PluginPermissions| {
            let lua = Lua::new();
            let reach = PluginReach::new(perms.clone());
            let t =
                create_ui_table(&lua, Some(tx.clone()), Arc::from(INPUT_PLUGIN), reach).unwrap();
            lua.globals().set("ui", t).unwrap();
            smol::block_on(
                lua.load(format!("return ui.action('{name}')"))
                    .eval_async::<Option<bool>>(),
            )
            .map_err(|e| e.to_string())
        };
        let denied = run(&PluginPermissions::denied());
        let Some(perm) = needed else {
            assert_eq!(denied, Ok(Some(true)));
            return;
        };
        let err = denied.unwrap_err();
        assert!(err.contains(&denied_error(perm).to_string()), "{err}");
        let mut granted = PluginPermissions::denied();
        granted.set(perm, true);
        assert_eq!(run(&granted), Ok(Some(true)));
    }
}
